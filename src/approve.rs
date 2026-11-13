//! `atelier approve <branch> <commit>` (DESIGN §3): records that `<commit>` was reviewed (or
//! deliberately not reviewed) and may be merged.
//!
//! Under the branch's lock: the record as every reading command sees it (a pending follow
//! settled, the base still in the branch); the commit is the branch's tip now; the branch holds
//! no merge commit of its own and is current (DESIGN §2.4, transitively up to the integration
//! branch); it is checked out in exactly one worktree, which is neither the main checkout nor the
//! gate. That worktree's canonical path and file-system identity are recorded: it is the only
//! worktree `clean` may later delete for this branch. Approving again replaces the record.

use std::path::Path;

use crate::exit::{Exit, Failure, Result};
use crate::fsutil;
use crate::gate::gate_worktree;
use crate::git;
use crate::ledger::Approval;
use crate::locks::{HeldLock, LockKey};
use crate::names;
use crate::preflight::{CONVERTERS, Context};
use crate::records::MergeFacts;
use crate::stack::{Stack, short};
use crate::state::CURRENT_FORMAT;

/// The full commit an argument names; refuses (4) one that names no commit.
pub fn resolve_commit(dir: &Path, arg: &str) -> Result<String> {
    if arg.is_empty() || arg.starts_with('-') {
        return Err(Failure::usage(format!("not a commit: {arg:?}")));
    }
    let out = git::output(
        dir,
        ["rev-parse", "-q", "--verify", &format!("{arg}^{{commit}}")],
    )?;
    if !out.status.success() {
        return Err(Failure::refused(format!("{arg} names no commit")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Refuses (4) a branch that has a merge commit of its own or is not current.
pub fn require_mergeable(stack: &Stack, dir: &Path, branch: &str) -> Result<()> {
    let (rec, tip) = stack.started(branch)?;
    let merges = stack.own_merges(&rec, &tip)?;
    if !merges.is_empty() {
        return Err(Failure::refused(format!(
            "{branch} contains a merge commit of its own ({}): replace the merge by a rebase of its own commits (atelier follow)",
            merges
                .iter()
                .map(|m| short(dir, m))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    stack.require_current(branch)
}

pub fn run(ctx: &Context, branch: &str, commit: &str) -> Result<Exit> {
    let root = &ctx.checkout.root;
    names::validate_branch(root, branch)?;
    let v = ctx.state.versioned(CURRENT_FORMAT, CONVERTERS)?;
    let _lock = HeldLock::acquire(&ctx.state, LockKey::branch(branch))?;
    let stack = Stack::new(&ctx.checkout, &ctx.state, &ctx.settings.integration_branch);
    let (rec, tip) = stack.prepare(branch)?;
    let wanted = resolve_commit(root, commit)?;
    if wanted != tip {
        return Err(Failure::refused(format!(
            "{branch} is at {}, not {}: approve the commit that was reviewed, at the branch's tip",
            short(root, &tip),
            short(root, &wanted)
        )));
    }
    require_mergeable(&stack, root, branch)?;

    let refname = names::branch_ref(branch);
    let on: Vec<_> = git::worktree_list(root)?
        .into_iter()
        .filter(|w| w.branch.as_deref() == Some(refname.as_str()))
        .collect();
    let wt = match on.as_slice() {
        [] => {
            return Err(Failure::refused(format!(
                "{branch} is not checked out in any worktree: approve it where it was built"
            )));
        }
        [one] => one,
        many => {
            return Err(Failure::refused(format!(
                "{branch} is checked out in {} worktrees ({}): it must be in exactly one",
                many.len(),
                many.iter()
                    .map(|w| w.path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    };
    let main = git::canonical(root);
    let path = git::canonical(&wt.path);
    if wt.main || path == main {
        return Err(Failure::refused(format!(
            "{branch} is checked out in the main checkout: approve a branch in its own worktree"
        )));
    }
    if path == git::canonical(&gate_worktree(&ctx.checkout)) {
        return Err(Failure::refused(format!(
            "{branch} is checked out in the gate worktree {}: atelier keeps it detached",
            path.display()
        )));
    }
    if wt.prunable {
        return Err(Failure::refused(format!(
            "{branch}'s worktree {} is missing",
            wt.path.display()
        )));
    }
    let facts = MergeFacts::of_worktree(&tip, &wt.path)?;
    Approval {
        branch: branch.to_owned(),
        lifetime: rec.lifetime.clone(),
        commit: tip.clone(),
        worktree: facts.worktree.clone(),
        dev: facts.dev,
        ino: facts.ino,
        approved: fsutil::rfc3339(fsutil::now_secs()),
    }
    .write(&v)?;
    println!(
        "approved {branch} {} (worktree {})",
        short(root, &tip),
        facts.worktree.display()
    );
    Ok(Exit::Ok)
}
