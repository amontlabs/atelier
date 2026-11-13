//! `atelier tmp [--short] -- <command>…` (DESIGN §3): runs a command with a registered temp
//! folder in `ATELIER_TMP`, for the branch checked out in the worktree it runs in.
//!
//! The branch comes from this worktree's `HEAD`, never from an argument, so a worker cannot
//! register a folder against another branch. It must have an `active` record; a detached `HEAD`,
//! the main checkout and the gate worktree are refused (4). The branch's lock is held from before
//! the folder exists until the command and its processes are done, so `clean` refuses while
//! anything uses the folder; there is no way to get a folder without holding it. The folder is
//! registered (canonical path, file-system identity) under the branch's lifetime id before the
//! command starts, and stays after it, for inspection, until `clean`.
//!
//! Folders: `<temp dir>/atelier/<repo id>/<branch key>/<id>/`, or with `--short`
//! `/tmp/atl-<id>/`, which keeps socket paths under macOS's 104 bytes. Mode 0700. A program that
//! creates something outside the folder (a virtual machine, a container) writes one line per thing
//! into `<folder>/leftovers`, and removes the line when the thing is gone: `clean` refuses while
//! that file is not empty.

use std::fs;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::clean::{gate_worktree, repo_id, short_name};
use crate::exit::{Exit, Failure, IoContext, Result};
use crate::fsutil;
use crate::git::canonical;
use crate::locks::{HeldLock, LockKey};
use crate::names;
use crate::preflight::Context;
use crate::records::TmpEntry;
use crate::runs::{Outcome, Run, RunKind};
use crate::stack::Stack;

/// The variable the command finds its folder in.
pub const TMP_VAR: &str = "ATELIER_TMP";

pub fn run(ctx: &Context, cwd: &Path, short: bool, command: &[String]) -> Result<Exit> {
    let checkout = &ctx.checkout;
    let Some(program) = command.first() else {
        return Err(Failure::usage("tmp: name a command after --"));
    };
    if program.starts_with('-') {
        return Err(Failure::usage(format!(
            "tmp: a program name cannot start with '-': {program:?}"
        )));
    }
    if checkout.is_main_worktree() {
        return Err(Failure::refused(
            "tmp runs in the worktree of a branch, not in the main checkout",
        ));
    }
    if canonical(&checkout.root) == gate_worktree(checkout) {
        return Err(Failure::refused(
            "tmp does not run in the gate worktree: it belongs to merge",
        ));
    }
    let branch = crate::follow::current_branch(checkout, "tmp")?;
    let lock = HeldLock::acquire(&ctx.state, LockKey::branch(&branch))?;
    let stack = Stack::new(checkout, &ctx.state, &ctx.settings.integration_branch);
    let (rec, _tip) = stack.started(&branch)?;

    let id = new_entry_id()?;
    let (folder, shown) = make_folder(ctx, &branch, &id, short)?;
    let entry = TmpEntry::of(&folder)?;
    if let Err(e) = entry.register(&ctx.state, &rec.lifetime, &id) {
        let _ = fs::remove_dir(&folder);
        return Err(e);
    }

    let text = format!("atelier tmp -- {}", command.join(" "));
    let mut run = Run::start_held(&ctx.state, lock, RunKind::Tmp, &text, &checkout.root)?;
    run.set_branch(Some(branch.clone()));
    let env = [(TMP_VAR.to_owned(), shown.to_string_lossy().into_owned())];
    let step = run.step("command", command, cwd, &env);
    let step = match step {
        Ok(s) => s,
        Err(e) => {
            run.fail(e.message.clone());
            let _ = run.finish(false);
            return Err(e);
        }
    };
    let log = run.dir().join(&step.log);
    if let Ok(text) = fs::read(&log) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(&text);
        let _ = out.flush();
    }
    let passed = step.passed();
    let run_id = run.id().to_owned();
    let result = run.finish(passed)?;
    let how = match (step.exit, step.signal) {
        (Some(c), _) => format!("exit {c}"),
        (None, Some(s)) => format!("signal {s}"),
        _ => format!("{:?}", step.status),
    };
    for s in &result.survivors {
        eprintln!("tmp: process left behind: {s}");
    }
    for step in &result.steps {
        for s in step.left.iter().chain(&step.unstoppable) {
            eprintln!("tmp: process left behind: {s}");
        }
    }
    println!(
        "tmp: {} — {how}, folder {}, run {run_id}",
        if result.outcome == Outcome::Passed {
            "GREEN"
        } else {
            "RED"
        },
        shown.display()
    );
    Ok(if result.outcome == Outcome::Passed {
        Exit::Ok
    } else {
        Exit::Fail
    })
}

fn new_entry_id() -> Result<String> {
    Ok(fsutil::random_hex(6)?)
}

/// Creates the folder, mode 0700, new (an existing path is never reused). Returns the real path
/// and the path to hand to the command (for `--short`, `/tmp/atl-<id>` as written, which is
/// shorter than macOS's `/private/tmp/…`).
fn make_folder(ctx: &Context, branch: &str, id: &str, short: bool) -> Result<(PathBuf, PathBuf)> {
    if short {
        let shown = Path::new("/tmp").join(short_name(id));
        make_dir(&shown)?;
        let real = canonical(&shown);
        return Ok((real, shown));
    }
    let temp = std::env::temp_dir();
    if !temp.is_absolute() {
        return Err(Failure::refused(format!(
            "TMPDIR is not an absolute path: {}",
            temp.display()
        )));
    }
    let base = canonical(&temp)
        .join("atelier")
        .join(repo_id(&ctx.checkout))
        .join(names::key(branch));
    let mut at = canonical(&temp);
    for part in ["atelier"]
        .into_iter()
        .map(str::to_owned)
        .chain([repo_id(&ctx.checkout), names::key(branch)])
    {
        at.push(part);
        ensure_private_dir(&at)?;
    }
    debug_assert_eq!(at, base);
    let folder = base.join(id);
    make_dir(&folder)?;
    Ok((folder.clone(), folder))
}

/// A folder of ours that exists or is created now: a real folder (never a link) owned by this
/// user.
fn ensure_private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) => {
            // SAFETY: geteuid has no preconditions.
            if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } {
                return Err(Failure::refused(format!(
                    "{} exists and is not a folder owned by this user: atelier will not put temp folders in it",
                    path.display()
                )));
            }
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::NotFound => make_dir(path),
        Err(e) => Err(Failure::refused(format!("{}: {e}", path.display()))),
    }
}

/// `mkdir` with mode 0700; refuses a path that exists (a link included).
fn make_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .ctx(path.display())?;
    // The umask can only remove bits, but say it exactly.
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).ctx(path.display())
}
