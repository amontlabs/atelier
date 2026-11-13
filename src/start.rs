//! `atelier start <branch> [--on <lower>] [--path <worktree>]` (DESIGN §2.4): creates the branch
//! at the tip of its lower branch and records what it is built on.
//!
//! Where the branch is checked out: in a new worktree at `--path`; otherwise in the worktree
//! `start` runs in, which must be a fresh detached linked worktree (as T3 creates them).
//! Refused (4): a branch that exists, a name whose old record is kept for a dependent, a lower
//! branch atelier did not start or that is merged, and a `--on` that would make a cycle.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use crate::exit::{Exit, Failure, Result};
use crate::git;
use crate::locks::{HeldLock, LockKey};
use crate::names;
use crate::preflight::Context;
use crate::records::{self, BranchRecord, BranchState};
use crate::refs::{self, Expected};
use crate::stack::{self, Stack};

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

    if stack.tip(&branch)?.is_some() {
        return Err(Failure::refused(format!("branch {branch} exists")));
    }
    let old = stack.record(&branch)?;
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

    let (base, lower_lifetime) = if lower == integration {
        (stack.integration_tip()?, None)
    } else {
        let rec = stack.record(&lower)?;
        match rec {
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
        let (rec, tip) = stack.started(&lower)?;
        (tip, Some(rec.lifetime))
    };

    // Where the branch will be checked out, checked before anything is written.
    let place = match path {
        Some(p) => Place::NewWorktree(cwd.join(p)),
        None => {
            fresh_detached(ctx)?;
            Place::Here
        }
    };

    let refname = names::branch_ref(&branch);
    refs::update(root, &refname, &base, &Expected::Absent, "atelier start")?;
    let record = BranchRecord {
        name: branch.clone(),
        lifetime: records::new_lifetime_id()?,
        lower_is_integration: lower == integration,
        lower: lower.clone(),
        lower_lifetime,
        base: base.clone(),
        state: BranchState::Active,
        pending: None,
        later: toml::Table::new(),
    };
    let undo = |e: Failure| -> Failure {
        let restored = match &old {
            Some(o) => o.write(&ctx.state),
            None => match fs::remove_file(ctx.state.branch_record_path(&branch)) {
                Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            },
        };
        let deleted = refs::delete(root, &refname, &base);
        match (restored, deleted) {
            (Ok(()), Ok(())) => e,
            (r, d) => e.context(format!(
                "and undoing start failed ({})",
                [r.err(), d.err()]
                    .into_iter()
                    .flatten()
                    .map(|f| f.message)
                    .collect::<Vec<_>>()
                    .join("; ")
            )),
        }
    };
    if let Err(e) = record.write(&ctx.state) {
        return Err(undo(e));
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
        Place::Here => git_step(
            root,
            &["switch", "-q", "--no-guess", &branch].map(std::ffi::OsStr::new),
        ),
    };
    if let Err(e) = checked_out {
        return Err(undo(e));
    }

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
    NewWorktree(std::path::PathBuf),
    Here,
}

/// Refuses (4) unless this is a linked worktree on a detached `HEAD` with nothing uncommitted.
fn fresh_detached(ctx: &Context) -> Result<()> {
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
    Ok(())
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
