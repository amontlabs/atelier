//! `atelier start <branch> [--on <lower>] [--path <worktree>]`: creates the branch
//! at the tip of its lower branch and records what it is built on. `atelier start <branch>
//! --abandon` only undoes an earlier start of that name that did not finish.
//!
//! Where the branch is checked out: in a new worktree at `--path`; otherwise in the worktree
//! `start` runs in, which must be a fresh detached linked worktree (as T3 creates them): no
//! uncommitted change, and no commit that only its `HEAD` holds (one no branch, tag or remote
//! branch reaches). Refused (4): a branch that exists, a name whose old record
//! is kept for a dependent, a lower branch atelier did not start, that is merged, or that
//! cannot be read (a rebase open, its base lost), and a `--on` that would make a cycle.
//!
//! The steps, each one undoable by a later `start` of the same name if atelier dies between
//! them (`docs/FROZEN-LAYER.md`): write the record with `starting` set, create the branch ref,
//! check the branch out, clear `starting`. A failed step is undone at once when the branch is
//! not in use: the ref is deleted only while it still holds the base, and never while a
//! registered worktree has the branch checked out or a rebase or bisect of it open (git's own
//! sense of in use). Otherwise the ref and the unfinished record stay, and the message gives the
//! commands that finish or undo the start.

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
use crate::stack::{self, Stack};
use crate::state::State;

pub fn run(
    ctx: &Context,
    cwd: &Path,
    branch: &str,
    on: Option<&str>,
    path: Option<&Path>,
    abandon: bool,
) -> Result<Exit> {
    let root = &ctx.checkout.root;
    let branch = names::validate_branch(root, branch)?;
    let integration = ctx.settings.integration_branch.clone();
    let lower = names::validate_branch(root, on.unwrap_or(&integration))?;
    let stack = Stack::new(&ctx.checkout, &ctx.state, &integration);
    let _lock = HeldLock::acquire(&ctx.state, LockKey::branch(&branch))?;
    if abandon {
        let start = Start {
            root,
            state: &ctx.state,
            stack: &stack,
            branch: &branch,
            integration: &integration,
        };
        start.abandon(stack.record(&branch)?.as_ref())?;
        return Ok(Exit::Ok);
    }

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
            let mut msg = format!(
                "the earlier {branch} is still the lower branch of {}: pick another name, or let {} follow first",
                deps.join(", "),
                if deps.len() == 1 { "it" } else { "them" }
            );
            for d in &deps {
                if stack.record(d)?.is_some_and(|r| r.starting.is_some()) {
                    msg.push_str(&format!(
                        "; the start of {d} did not finish: atelier start {d} --abandon undoes it"
                    ));
                }
            }
            return Err(Failure::refused(msg));
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
        fresh_detached(ctx, &[&base, &stack.integration_tip()?])?;
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
    /// for the same lower branch and place), false when it undid it (the branch is not in use,
    /// and it is gone or still at the recorded base). Refuses (4) anything else, leaving the ref
    /// and the record, and gives the commands that finish or undo it.
    fn recover(&self, rec: &BranchRecord, lower: &str, place: &Place) -> Result<bool> {
        let branch = self.branch;
        let starting = rec.starting.as_ref().expect("an unfinished start");
        let uses = self.uses()?;
        let tip = self.stack.tip(branch)?;
        let recorded = Path::new(&starting.worktree);
        if !uses.checked.is_empty() {
            let same_request = rec.lower == lower
                && same_path(place.path(), recorded)
                && starting.new_worktree == matches!(place, Place::NewWorktree(_));
            if tip.is_some()
                && same_request
                && uses.busy.is_empty()
                && uses.checked.iter().any(|p| same_path(p, recorded))
            {
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
        }
        if !uses.is_empty() {
            return Err(Failure::refused(format!(
                "an earlier start of {branch} did not finish and {}",
                self.in_use_advice(rec, &uses)
            )));
        }
        match &tip {
            None => {}
            Some(t) if *t == rec.base => refs::delete(self.root, &refname(branch), &rec.base)?,
            Some(t) => {
                return Err(Failure::refused(format!(
                    "an earlier start of {branch} did not finish, and {}",
                    self.moved_advice(rec, t)
                )));
            }
        }
        remove_record(self.state, branch)?;
        println!("start: undid an earlier start of {branch} that did not finish");
        Ok(false)
    }

    /// `start --abandon`: removes the unfinished start of this name, deleting its ref when it
    /// still holds only the base. Refuses (4), changing nothing, when there is no unfinished
    /// start, when the branch is in use, or when it holds commits after its base.
    fn abandon(&self, rec: Option<&BranchRecord>) -> Result<()> {
        let branch = self.branch;
        let Some(rec) = rec.filter(|r| r.starting.is_some()) else {
            return Err(Failure::refused(format!(
                "{branch} has no unfinished start to abandon"
            )));
        };
        let uses = self.uses()?;
        if !uses.is_empty() {
            return Err(Failure::refused(format!(
                "cannot abandon the start of {branch}: {}",
                self.in_use_advice(rec, &uses)
            )));
        }
        match self.stack.tip(branch)? {
            None => {}
            Some(t) if t == rec.base => refs::delete(self.root, &refname(branch), &rec.base)?,
            Some(t) => {
                return Err(Failure::refused(format!(
                    "cannot abandon the start of {branch}: {}",
                    self.moved_advice(rec, &t)
                )));
            }
        }
        remove_record(self.state, branch)?;
        println!("start: abandoned the unfinished start of {branch}");
        Ok(())
    }

    /// Undoes this start after a failed step, as far as it is safe: never while the branch is
    /// in use, and the ref only when this start created it and it still holds the base. What
    /// cannot be undone stays with the unfinished record, and the failure gives the commands
    /// that finish or undo it.
    fn undo(
        &self,
        record: &BranchRecord,
        old: Option<&BranchRecord>,
        created_ref: bool,
        e: Failure,
    ) -> Failure {
        let branch = self.branch;
        let uses = match self.uses() {
            Ok(u) => u,
            Err(f) => return e.context(format!("and undoing start failed ({})", f.message)),
        };
        if !uses.is_empty() {
            return e.context(format!(
                "the start is kept unfinished: {}",
                self.in_use_advice(record, &uses)
            ));
        }
        if created_ref {
            match self.stack.tip(branch) {
                Ok(None) => {}
                Ok(Some(t)) if t == record.base => {
                    if let Err(f) = refs::delete(self.root, &refname(branch), &record.base) {
                        return e.context(format!(
                            "and undoing start failed ({}); run atelier start {branch} --abandon to undo it",
                            f.message
                        ));
                    }
                }
                Ok(Some(t)) => {
                    return e.context(format!(
                        "the start is kept unfinished: {}",
                        self.moved_advice(record, &t)
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
                "and undoing start failed ({}); run atelier start {branch} --abandon to undo it",
                f.message
            )),
        }
    }

    /// Where the branch is in use, in git's sense: checked out in a registered worktree, or a
    /// rebase or a bisect of it open in one.
    fn uses(&self) -> Result<Uses> {
        let branch = self.branch;
        let mut busy = Vec::new();
        if let Some(wt) = self.stack.rebase_open(branch)? {
            busy.push(format!("a rebase of {branch} is open in {}", wt.display()));
        }
        if let Some(wt) = self.stack.bisect_open(branch)? {
            busy.push(format!("a bisect of {branch} is open in {}", wt.display()));
        }
        Ok(Uses {
            checked: worktrees_on(self.root, &refname(branch))?,
            busy,
        })
    }

    /// What to do about an unfinished start whose branch is in use: the command that finishes
    /// it, and the commands that free the branch so that `--abandon` can undo it.
    fn in_use_advice(&self, rec: &BranchRecord, uses: &Uses) -> String {
        let branch = self.branch;
        let starting = rec.starting.as_ref().expect("an unfinished start");
        let recorded = Path::new(&starting.worktree);
        let mut what = Vec::new();
        let mut free = Vec::new();
        if !uses.checked.is_empty() {
            what.push(format!(
                "{branch} is checked out in {}",
                list_paths(&uses.checked)
            ));
            for p in &uses.checked {
                free.push(if starting.new_worktree && same_path(p, recorded) {
                    format!("git worktree remove {}", quote(&p.to_string_lossy()))
                } else {
                    format!("git -C {} switch --detach", quote(&p.to_string_lossy()))
                });
            }
        }
        if !uses.busy.is_empty() {
            what.extend(uses.busy.iter().cloned());
            free.push("finish or abort it there (git rebase --abort, git bisect reset)".into());
        }
        format!(
            "{}. To finish the start, run {}; to undo it, {}, then run atelier start {branch} --abandon",
            what.join(", and "),
            self.command(rec),
            free.join(", ")
        )
    }

    /// What to do about an unfinished start whose branch, in use nowhere, has moved from its
    /// base to `tip`: how to keep that work, and how to undo the start.
    fn moved_advice(&self, rec: &BranchRecord, tip: &str) -> String {
        let branch = self.branch;
        let starting = rec.starting.as_ref().expect("an unfinished start");
        let path = quote(&starting.worktree);
        let check_out = if starting.new_worktree {
            format!("git worktree add {path} {branch}")
        } else {
            format!("git -C {path} switch {branch}")
        };
        format!(
            "{branch} is at {}, not at the base {} it recorded. To keep that work in {branch}, check it out again ({check_out}) and run {} to finish the start; to keep it under another name, run git branch <new-name> {branch} first. To undo the start, run git branch -D {branch}, then atelier start {branch} --abandon",
            stack::short(self.root, tip),
            stack::short(self.root, &rec.base),
            self.command(rec)
        )
    }

    /// The `start` command that finishes `rec`'s start: with `--path` for a new worktree, run
    /// from that worktree for a start in place.
    fn command(&self, rec: &BranchRecord) -> String {
        let mut c = format!("atelier start {}", self.branch);
        if rec.lower != self.integration {
            c.push_str(&format!(" --on {}", rec.lower));
        }
        match &rec.starting {
            Some(s) if s.new_worktree => {
                c.push_str(&format!(" --path {}", quote(&s.worktree)));
                c
            }
            Some(s) => format!("cd {} && {c}", quote(&s.worktree)),
            None => c,
        }
    }
}

/// Where a branch is in use: the worktrees that have it checked out, and the rebases and
/// bisects of it open (described).
struct Uses {
    checked: Vec<PathBuf>,
    busy: Vec<String>,
}

impl Uses {
    fn is_empty(&self) -> bool {
        self.checked.is_empty() && self.busy.is_empty()
    }
}

fn refname(branch: &str) -> String {
    names::branch_ref(branch)
}

/// `s` as one shell word: as it is when it has no character the shell treats specially,
/// otherwise in single quotes.
fn quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+,:@%=".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Refuses (4) unless this is a linked worktree on a detached `HEAD`, with nothing uncommitted
/// and no commit of its own: one reachable from `HEAD` and from no branch, tag, remote branch or
/// commit in `held` (the lower branch's tip and the integration branch's tip). Only such commits
/// would be lost when the branch is checked out here; a worktree detached at any commit some ref
/// holds (an older or a newer integration commit) is fresh.
fn fresh_detached(ctx: &Context, held: &[&str]) -> Result<()> {
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
    if let Some(head) = ctx.checkout.head_commit()? {
        const MAX: usize = 10;
        let max = format!("--max-count={}", MAX + 1);
        let mut args = vec![
            "rev-list",
            "--abbrev-commit",
            &max,
            &head,
            "--not",
            "--branches",
            "--tags",
            "--remotes",
        ];
        args.extend(held);
        let own = git::run(root, args)?;
        let own: Vec<&str> = own.lines().collect();
        if !own.is_empty() {
            let mut named = own.iter().take(MAX).copied().collect::<Vec<_>>().join(", ");
            if own.len() > MAX {
                named.push_str(", …");
            }
            return Err(Failure::refused(format!(
                "this detached worktree is not fresh: it has commits no branch holds ({named}): put them on a branch (git switch -c <name>) or give --path <worktree>"
            )));
        }
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
