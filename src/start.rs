//! `atelier start <branch> [--on <lower>] [--path <worktree>]` (DESIGN §2.4): creates the branch
//! at the tip of its lower branch and records what it is built on.
//!
//! Where the branch is checked out: in a new worktree at `--path`; otherwise in the worktree
//! `start` runs in, which must be a fresh detached linked worktree (as T3 creates them) whose
//! commits the lower branch contains. Refused (4): a branch that exists, a name whose old record
//! is kept for a dependent, a lower branch atelier did not start, that is merged, or that
//! cannot be read (a rebase open, its base lost), and a `--on` that would make a cycle.
//!
//! The steps, each one undoable by a later `start` of the same name if atelier dies between
//! them (`docs/FROZEN-LAYER.md`): write the record with `starting` set, create the branch ref,
//! check the branch out, clear `starting`. A failed step is undone at once when nothing of it
//! is checked out: the ref is deleted only while it still holds the base, and never while a
//! registered worktree has the branch checked out. Otherwise the ref and the unfinished record
//! stay, and the message says how to finish or undo the start.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::exit::{Exit, Failure, Result};
use crate::git;
use crate::locks::{HeldLock, LockKey};
use crate::names;
use crate::preflight::Context;
use crate::records::{self, BranchRecord, BranchState, Starting};
use crate::refs::{self, Expected};
use crate::stack::{self, Stack, is_ancestor};
use crate::state::State;

pub fn run(
    ctx: &Context,
    cwd: &Path,
    branch: &str,
    on: Option<&str>,
    path: Option<&Path>,
) -> Result<Exit> {
    let root = &ctx.checkout.root;
    let branch = names::validate_branch(root, branch)?;
    let integration = ctx.settings.integration_branch.clone();
    let lower = names::validate_branch(root, on.unwrap_or(&integration))?;
    let stack = Stack::new(&ctx.checkout, &ctx.state, &integration);
    let _lock = HeldLock::acquire(&ctx.state, LockKey::branch(&branch))?;

    // Where the branch will be checked out.
    let place = match path {
        Some(p) => Place::NewWorktree(absolute(&cwd.join(p))),
        None => Place::Here(root.clone()),
    };
    let start = Start {
        root,
        state: &ctx.state,
        stack: &stack,
        branch: &branch,
        integration: &integration,
    };

    let mut old = stack.record(&branch)?;
    if let Some(rec) = old.as_ref().filter(|r| r.starting.is_some()) {
        if start.recover(rec, &lower, &place)? {
            return Ok(Exit::Ok);
        }
        old = None;
    }

    if stack.tip(&branch)?.is_some() {
        return Err(Failure::refused(format!("branch {branch} exists")));
    }
    if let Some(old) = &old {
        let deps = stack.dependents(&branch, &old.lifetime)?;
        if !deps.is_empty() {
            return Err(Failure::refused(format!(
                "the earlier {branch} is still the lower branch of {}: pick another name, or let {} follow first",
                deps.join(", "),
                if deps.len() == 1 { "it" } else { "them" }
            )));
        }
    }
    stack.check_no_cycle(&branch, &lower)?;

    // The lower branch is read as every command reads a record, under its own lock: no rebase
    // of it open in any worktree, a pending follow settled, its base in it.
    let _lower_lock;
    let (base, lower_lifetime) = if lower == integration {
        (stack.integration_tip()?, None)
    } else {
        match stack.record(&lower)? {
            Some(r) if r.state == BranchState::Active => {}
            Some(_) => {
                return Err(Failure::refused(format!(
                    "{lower} is merged: start {branch} on {integration}"
                )));
            }
            None => {
                return Err(Failure::refused(format!(
                    "{lower} was not started by atelier: build on a branch atelier started, or on {integration}"
                )));
            }
        }
        _lower_lock = HeldLock::acquire(&ctx.state, LockKey::branch(&lower))?;
        let (rec, tip) = stack.prepare(&lower)?;
        (tip, Some(rec.lifetime))
    };

    // Where the branch will be checked out, checked before anything is written.
    if let Place::Here(_) = &place {
        fresh_detached(ctx, &lower, &base)?;
    }
    let worktree = place
        .path()
        .to_str()
        .ok_or_else(|| {
            Failure::refused(format!(
                "{} is not a UTF-8 path; atelier records only UTF-8 paths",
                place.path().display()
            ))
        })?
        .to_owned();

    let mut record = BranchRecord {
        name: branch.clone(),
        lifetime: records::new_lifetime_id()?,
        lower_is_integration: lower == integration,
        lower: lower.clone(),
        lower_lifetime,
        base: base.clone(),
        state: BranchState::Active,
        pending: None,
        starting: Some(Starting {
            worktree,
            new_worktree: matches!(place, Place::NewWorktree(_)),
            later: toml::Table::new(),
        }),
        later: toml::Table::new(),
    };
    // Written atomically: on failure the old record is still there and nothing else moved.
    record.write(&ctx.state)?;

    let refname = names::branch_ref(&branch);
    if let Err(e) = refs::update(root, &refname, &base, &Expected::Absent, "atelier start") {
        return Err(start.undo(&record, old.as_ref(), false, e));
    }
    let checked_out = match &place {
        Place::NewWorktree(p) => git_step(
            root,
            &[
                "worktree".as_ref(),
                "add".as_ref(),
                p.as_os_str(),
                branch.as_ref(),
            ],
        ),
        Place::Here(_) => git_step(
            root,
            &["switch", "-q", "--no-guess", &branch].map(std::ffi::OsStr::new),
        ),
    };
    if let Err(e) = checked_out {
        return Err(start.undo(&record, old.as_ref(), true, e));
    }
    record.starting = None;
    record.write(&ctx.state).map_err(|e| {
        e.context(format!(
            "{branch} is checked out but its record still says the start did not finish: run atelier start {branch} again to finish it"
        ))
    })?;

    println!(
        "start: {branch} on {lower} at {}",
        stack::short(root, &base)
    );
    if let Place::NewWorktree(p) = &place {
        println!("worktree: {}", git::canonical(p).display());
    }
    Ok(Exit::Ok)
}

enum Place {
    NewWorktree(PathBuf),
    /// The worktree `start` runs in.
    Here(PathBuf),
}

impl Place {
    fn path(&self) -> &Path {
        match self {
            Place::NewWorktree(p) | Place::Here(p) => p,
        }
    }
}

struct Start<'a> {
    root: &'a Path,
    state: &'a State,
    stack: &'a Stack<'a>,
    branch: &'a str,
    integration: &'a str,
}

impl Start<'_> {
    /// Finishes or undoes an earlier start of this name that did not finish. Returns true when
    /// it finished it (the branch is checked out in the recorded worktree and this start asks
    /// for the same lower branch and place), false when it undid it (no worktree has the branch
    /// checked out, and the branch is gone or still at the recorded base). Refuses (4) anything
    /// else, leaving the ref and the record, and says what to do.
    fn recover(&self, rec: &BranchRecord, lower: &str, place: &Place) -> Result<bool> {
        let branch = self.branch;
        let starting = rec.starting.as_ref().expect("an unfinished start");
        let refname = names::branch_ref(branch);
        let checked = worktrees_on(self.root, &refname)?;
        let tip = self.stack.tip(branch)?;
        let recorded = Path::new(&starting.worktree);
        let again = self.command(rec);
        if !checked.is_empty() {
            let same_request = rec.lower == lower
                && same_path(place.path(), recorded)
                && starting.new_worktree == matches!(place, Place::NewWorktree(_));
            if tip.is_some() && same_request && checked.iter().any(|p| same_path(p, recorded)) {
                let mut done = rec.clone();
                done.starting = None;
                done.write(self.state)?;
                println!(
                    "start: finished the interrupted start of {branch} on {} at {}",
                    rec.lower,
                    stack::short(self.root, &rec.base)
                );
                if starting.new_worktree {
                    println!("worktree: {}", git::canonical(recorded).display());
                }
                return Ok(true);
            }
            return Err(Failure::refused(format!(
                "an earlier start of {branch} did not finish and {branch} is checked out in {}: to finish it, run {again}; to undo it, remove that worktree (git worktree remove --force <path>), then run atelier start {branch} again",
                list_paths(&checked)
            )));
        }
        match &tip {
            None => {}
            Some(t) if *t == rec.base => refs::delete(self.root, &refname, &rec.base)?,
            Some(t) => {
                return Err(Failure::refused(format!(
                    "an earlier start of {branch} did not finish, and {branch} is at {}, not at the base {} it recorded: delete it (git branch -D {branch}) if nothing in it is needed, then run atelier start {branch} again",
                    stack::short(self.root, t),
                    stack::short(self.root, &rec.base)
                )));
            }
        }
        remove_record(self.state, branch)?;
        println!("start: undid an earlier start of {branch} that did not finish");
        Ok(false)
    }

    /// Undoes this start after a failed step, as far as it is safe: never while a registered
    /// worktree has the branch checked out, and the ref only when this start created it and it
    /// still holds the base. What cannot be undone stays with the unfinished record, and the
    /// failure says what to do.
    fn undo(
        &self,
        record: &BranchRecord,
        old: Option<&BranchRecord>,
        created_ref: bool,
        e: Failure,
    ) -> Failure {
        let branch = self.branch;
        let refname = names::branch_ref(branch);
        let checked = match worktrees_on(self.root, &refname) {
            Ok(c) => c,
            Err(f) => return e.context(format!("and undoing start failed ({})", f.message)),
        };
        if !checked.is_empty() {
            return e.context(format!(
                "{branch} is checked out in {} all the same; the start is kept unfinished. Check that worktree, then run {} to finish it; or remove it (git worktree remove --force <path>) and run atelier start {branch} again to undo it",
                list_paths(&checked),
                self.command(record)
            ));
        }
        if created_ref {
            match self.stack.tip(branch) {
                Ok(None) => {}
                Ok(Some(t)) if t == record.base => {
                    if let Err(f) = refs::delete(self.root, &refname, &record.base) {
                        return e.context(format!(
                            "and undoing start failed ({}); run atelier start {branch} again to undo it",
                            f.message
                        ));
                    }
                }
                Ok(Some(t)) => {
                    return e.context(format!(
                        "{branch} moved to {} meanwhile and is kept, with its unfinished record: delete it (git branch -D {branch}) if nothing in it is needed, then run atelier start {branch} again",
                        stack::short(self.root, &t)
                    ));
                }
                Err(f) => {
                    return e.context(format!("and undoing start failed ({})", f.message));
                }
            }
        }
        let restored = match old {
            Some(o) => o.write(self.state),
            None => remove_record(self.state, branch),
        };
        match restored {
            Ok(()) => e,
            Err(f) => e.context(format!(
                "and undoing start failed ({}); run atelier start {branch} again to undo it",
                f.message
            )),
        }
    }

    /// The `start` command that finishes `rec`'s start.
    fn command(&self, rec: &BranchRecord) -> String {
        let mut c = format!("atelier start {}", self.branch);
        if rec.lower != self.integration {
            c.push_str(&format!(" --on {}", rec.lower));
        }
        match &rec.starting {
            Some(s) if s.new_worktree => c.push_str(&format!(" --path {}", s.worktree)),
            Some(s) => c.push_str(&format!(" in {}", s.worktree)),
            None => {}
        }
        c
    }
}

/// Refuses (4) unless this is a linked worktree on a detached `HEAD`, with nothing uncommitted
/// and no commit that the lower branch's tip (`base`) does not contain: a worktree detached at
/// an older commit of the lower branch is fresh; one with its own commits is not.
fn fresh_detached(ctx: &Context, lower: &str, base: &str) -> Result<()> {
    let root = &ctx.checkout.root;
    if ctx.checkout.is_main_worktree() {
        return Err(Failure::refused(
            "start in the main checkout needs --path <worktree>",
        ));
    }
    let head = git::output(root, ["symbolic-ref", "-q", "--short", "HEAD"])?;
    if head.status.success() {
        return Err(Failure::refused(format!(
            "this worktree is on {}: give --path <worktree>, or run start in a fresh detached worktree",
            String::from_utf8_lossy(&head.stdout).trim()
        )));
    }
    let changes = crate::follow::uncommitted(root)?;
    if !changes.is_empty() {
        return Err(Failure::refused(format!(
            "this detached worktree is not fresh: it has uncommitted changes ({})",
            changes.join(", ")
        )));
    }
    if let Some(head) = ctx.checkout.head_commit()?
        && !is_ancestor(root, &head, base)?
    {
        const MAX: usize = 10;
        let own = git::run(
            root,
            [
                "rev-list",
                "--abbrev-commit",
                &format!("--max-count={}", MAX + 1),
                &head,
                &format!("^{base}"),
            ],
        )?;
        let own: Vec<&str> = own.lines().collect();
        let mut named = own.iter().take(MAX).copied().collect::<Vec<_>>().join(", ");
        if own.len() > MAX {
            named.push_str(", …");
        }
        return Err(Failure::refused(format!(
            "this detached worktree is not fresh: it has commits {lower} does not contain ({named}): put them on a branch (git switch -c <name>) or give --path <worktree>"
        )));
    }
    Ok(())
}

/// The registered worktrees whose `HEAD` names `refname` (prunable ones included: git still
/// counts the branch as checked out there).
fn worktrees_on(dir: &Path, refname: &str) -> Result<Vec<PathBuf>> {
    let out = git::output(dir, ["worktree", "list", "--porcelain", "-z"])?;
    if !out.status.success() {
        return Err(Failure::refused(format!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let mut found = Vec::new();
    let mut current: Option<PathBuf> = None;
    for field in out.stdout.split(|&b| b == 0) {
        let field = String::from_utf8_lossy(field);
        if let Some(p) = field.strip_prefix("worktree ") {
            current = Some(PathBuf::from(p));
        } else if field.strip_prefix("branch ") == Some(refname)
            && let Some(p) = &current
        {
            found.push(p.clone());
        }
    }
    Ok(found)
}

/// True when both paths name the same folder (each resolved when it exists).
fn same_path(a: &Path, b: &Path) -> bool {
    git::canonical(a) == git::canonical(b)
}

/// An absolute path, resolving the parent folders that exist: a worktree folder `start` is about
/// to create has none yet.
fn absolute(p: &Path) -> PathBuf {
    if let Ok(c) = fs::canonicalize(p) {
        return c;
    }
    match (p.parent(), p.file_name()) {
        (Some(parent), Some(name)) => absolute(parent).join(name),
        _ => p.to_path_buf(),
    }
}

fn list_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn remove_record(state: &State, branch: &str) -> Result<()> {
    let path = state.branch_record_path(branch);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Failure::refused(format!("{}: {e}", path.display()))),
    }
}

fn git_step(dir: &Path, args: &[&std::ffi::OsStr]) -> Result<()> {
    let out = git::output(dir, args)?;
    if out.status.success() {
        Ok(())
    } else {
        Err(Failure::refused(format!(
            "git {} failed: {}",
            args.iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}
