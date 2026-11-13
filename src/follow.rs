//! `atelier follow` (DESIGN §2.4, §8.2): rebases exactly this branch's own commits onto what it
//! is built on, when that moved.
//!
//! In order: settle an earlier pending follow (from ancestry alone); refuse uncommitted changes;
//! check the base is still in the branch (repairing it to the lower tip when that is); refuse a
//! merge commit of the branch's own; pick the target (the lower branch's tip, or the integration
//! branch once the lower is merged, or gone with its base in the integration branch); record
//! *pending*; `git rebase --onto <target> <base>`, never with an autostash. A conflict leaves the
//! rebase open and exits 5 with the files. With `[merge] resolver = "mergiraf"`, conflicted
//! `.rs`, `.ts`, `.tsx`, `.toml` and `.json` files are first given to `mergiraf solve`; a file it
//! resolves completely is staged in the open rebase and printed, any other is left as git's
//! conflict.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::exit::{Exit, Failure, IoContext, Result};
use crate::git::{self, Checkout};
use crate::locks::{HeldLock, LockKey};
use crate::preflight::Context;
use crate::records::Pending;
use crate::stack::{BaseCheck, FINISH_REBASE, Stack, is_ancestor, short};

/// The only files `mergiraf solve` is run on (DESIGN §8.2, P21).
pub const MERGIRAF_EXTENSIONS: &[&str] = &["rs", "ts", "tsx", "toml", "json"];

/// The resolver, found on `PATH`.
const MERGIRAF: &str = "mergiraf";

pub fn run(ctx: &Context) -> Result<Exit> {
    let root = &ctx.checkout.root;
    let branch = current_branch(&ctx.checkout)?;
    let integration = &ctx.settings.integration_branch;
    let _lock = HeldLock::acquire(&ctx.state, LockKey::branch(&branch))?;
    let stack = Stack::new(&ctx.checkout, &ctx.state, integration);
    let resolver = ctx.settings.merge.resolver.as_deref() == Some("mergiraf");
    if resolver {
        check_mergiraf()?;
    }

    let (rec, tip) = stack.started(&branch)?;
    let mut rec = stack.settle(rec, &tip)?;

    let changes = uncommitted(root)?;
    if !changes.is_empty() {
        return Err(Failure::refused(format!(
            "commit first: uncommitted changes in {}",
            list(&changes)
        )));
    }

    let target = stack.target(&rec);
    if let BaseCheck::Repair(new) = stack.check_base(&rec, &tip)? {
        println!(
            "follow: {branch} no longer contained its base; its base is now the {} tip {}",
            rec.lower,
            short(root, &new)
        );
        rec.base = new;
        rec.write(&ctx.state)?;
    }
    let target = target?;
    let merges = stack.own_merges(&rec, &tip)?;
    if !merges.is_empty() {
        return Err(Failure::refused(format!(
            "{branch} contains a merge commit of its own ({}): follow moves only linear work; replace the merge by a rebase of your own commits",
            merges
                .iter()
                .map(|m| short(root, m))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if target.repoint {
        println!(
            "follow: {} is {}; {branch} is now built on {integration}",
            rec.lower,
            if stack.record(&rec.lower)?.is_some_and(|l| {
                Some(&l.lifetime) == rec.lower_lifetime.as_ref()
                    && l.state != crate::records::BranchState::Active
            }) {
                "merged".to_owned()
            } else {
                format!("gone, and {integration} contains its base")
            }
        );
        rec.lower = integration.clone();
        rec.lower_is_integration = true;
        rec.lower_lifetime = None;
    }

    // Nothing to replay: the target moved forward from the base and is already below this
    // branch's own commits. A target that is not a descendant of the base (the lower branch was
    // reset or rewritten) always needs the rebase, even when the branch contains it: the
    // commits between them are no longer the lower branch's (REVIEW-4 finding 1).
    if is_ancestor(root, &rec.base, &target.commit)? && is_ancestor(root, &target.commit, &tip)? {
        let moved = rec.base != target.commit;
        if moved || target.repoint {
            rec.base = target.commit.clone();
            rec.write(&ctx.state)?;
        }
        println!(
            "follow: {branch} {} on {} ({} commits)",
            if moved { "now" } else { "already" },
            short(root, &target.commit),
            count(root, &target.commit, &tip)?
        );
        return Ok(Exit::Ok);
    }

    rec.pending = Some(Pending {
        target: target.commit.clone(),
        old_base: rec.base.clone(),
        later: toml::Table::new(),
    });
    rec.write(&ctx.state)?;

    let mut cmd = git::command(root);
    cmd.args([
        "-c",
        "rebase.autoStash=false",
        "-c",
        "rebase.updateRefs=false",
        "-c",
        "rebase.autoSquash=false",
    ]);
    if resolver {
        cmd.args(["-c", "merge.conflictStyle=diff3"]);
    }
    cmd.args([
        "rebase",
        "--no-autostash",
        "--no-update-refs",
        "--onto",
        &target.commit,
        &rec.base,
    ])
    .env("GIT_EDITOR", ":")
    .env("GIT_SEQUENCE_EDITOR", ":");
    let out = cmd
        .output()
        .map_err(|e| Failure::refused(format!("cannot run git: {e}")))?;

    if stack.rebase_open(&branch)?.is_some() {
        return Err(conflict(
            root,
            &branch,
            &target.commit,
            resolver,
            &String::from_utf8_lossy(&out.stderr),
        )?);
    }
    let new_tip = stack
        .tip(&branch)?
        .ok_or_else(|| Failure::refused(format!("{branch} disappeared during the rebase")))?;
    let rec = stack.settle(rec, &new_tip)?;
    if !out.status.success() {
        return Err(Failure::refused(format!(
            "git rebase did not start: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    println!(
        "follow: {branch} now on {} ({} commits)",
        short(root, &rec.base),
        count(root, &rec.base, &new_tip)?
    );
    Ok(Exit::Ok)
}

/// The branch `HEAD` names. Refuses (4) a detached `HEAD`, and an open rebase.
fn current_branch(checkout: &Checkout) -> Result<String> {
    let out = git::output(&checkout.root, ["symbolic-ref", "-q", "HEAD"])?;
    if out.status.success() {
        let r = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if let Some(b) = r.strip_prefix("refs/heads/") {
            return Ok(b.to_owned());
        }
    }
    for sub in ["rebase-merge", "rebase-apply"] {
        if let Ok(name) = fs::read_to_string(checkout.git_dir.join(sub).join("head-name")) {
            let name = name.trim_end();
            let b = name.strip_prefix("refs/heads/").unwrap_or(name);
            return Err(Failure::refused(format!("{b}: {FINISH_REBASE}")));
        }
    }
    Err(Failure::refused(
        "HEAD is detached: follow runs on a branch atelier started",
    ))
}

/// The exit-5 failure after a rebase stopped, with the files; runs the resolver first.
fn conflict(
    root: &Path,
    branch: &str,
    target: &str,
    resolver: bool,
    stderr: &str,
) -> Result<Failure> {
    let files = conflicted(root)?;
    let mut left = Vec::new();
    for f in &files {
        if resolver && mergiraf_applies(f) && mergiraf_solve(root, f)? {
            println!("resolved by mergiraf: {f}");
        } else {
            left.push(f.clone());
        }
    }
    let mut msg = format!(
        "follow: conflict rebasing {branch} onto {}",
        short(root, target)
    );
    if !left.is_empty() {
        msg.push_str(":\n");
        for f in &left {
            msg.push_str(&format!("  {f}\n"));
        }
    } else if files.is_empty() {
        msg.push_str(&format!("; git: {}\n", stderr.trim()));
    } else {
        msg.push_str(
            "; every conflicted file was resolved by mergiraf and staged: compile and read them\n",
        );
    }
    msg.push_str(
        "finish the rebase (git rebase --continue or --abort), then run atelier follow again",
    );
    Ok(Failure::new(Exit::Conflict, msg))
}

fn mergiraf_applies(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|e| MERGIRAF_EXTENSIONS.contains(&e))
}

/// Refuses (4) when the configured resolver cannot run.
fn check_mergiraf() -> Result<()> {
    let ok = Command::new(MERGIRAF)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        Ok(())
    } else {
        Err(Failure::refused(
            "merge.resolver is \"mergiraf\" but `mergiraf --version` does not run: install mergiraf or remove the setting",
        ))
    }
}

/// Runs `mergiraf solve` on one conflicted file. When it ends well and the file has no
/// conflict marker left, stages it and returns true; otherwise puts git's conflict back.
fn mergiraf_solve(root: &Path, rel: &str) -> Result<bool> {
    let path = root.join(rel);
    let original = fs::read(&path).ctx(path.display())?;
    let ok = Command::new(MERGIRAF)
        .args(["solve", "--keep-backup=false", "--"])
        .arg(rel)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let now = fs::read(&path).ok();
    if ok && now.as_deref().is_some_and(|t| !has_markers(t)) {
        let out = git::output(root, ["--literal-pathspecs", "add", "--", rel])?;
        if out.status.success() {
            return Ok(true);
        }
    }
    if now.as_deref() != Some(original.as_slice()) {
        fs::write(&path, &original).ctx(path.display())?;
    }
    Ok(false)
}

/// True when the text has a line that is a conflict marker.
fn has_markers(text: &[u8]) -> bool {
    text.split(|&b| b == b'\n').any(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        line.starts_with(b"<<<<<<<")
            || line.starts_with(b">>>>>>>")
            || line.starts_with(b"|||||||")
            || line == b"======="
    })
}

/// The unmerged paths, relative to the worktree root.
fn conflicted(root: &Path) -> Result<Vec<String>> {
    let out = git::output(root, ["diff", "--name-only", "--diff-filter=U", "-z"])?;
    Ok(out
        .stdout
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect())
}

/// Every uncommitted change: tracked changes, staged or not, and untracked files that are not
/// ignored. Paths relative to the worktree root.
pub fn uncommitted(root: &Path) -> Result<Vec<String>> {
    let out = git::output(
        root,
        [
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )?;
    if !out.status.success() {
        return Err(Failure::refused(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let mut paths = Vec::new();
    let mut entries = out.stdout.split(|&b| b == 0).filter(|e| !e.is_empty());
    while let Some(e) = entries.next() {
        if e.len() < 4 {
            continue;
        }
        paths.push(String::from_utf8_lossy(&e[3..]).into_owned());
        // A rename or copy is followed by its source path.
        if matches!(e[0], b'R' | b'C') {
            entries.next();
        }
    }
    Ok(paths)
}

fn list(paths: &[String]) -> String {
    const MAX: usize = 10;
    let mut s = paths
        .iter()
        .take(MAX)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > MAX {
        s.push_str(&format!(" and {} more", paths.len() - MAX));
    }
    s
}

fn count(root: &Path, from: &str, to: &str) -> Result<String> {
    git::run(root, ["rev-list", "--count", &format!("{from}..{to}")])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_are_whole_marker_lines() {
        assert!(has_markers(b"a\n<<<<<<< HEAD\nb\n=======\nc\n>>>>>>> x\n"));
        assert!(has_markers(b"||||||| base\n"));
        assert!(!has_markers(b"let s = \"=======\";\n// <<<< not seven\n"));
    }

    #[test]
    fn mergiraf_runs_only_on_code_and_data_files() {
        for yes in [
            "src/a.rs",
            "ui/x.ts",
            "ui/x.tsx",
            "Cargo.toml",
            "a/package.json",
        ] {
            assert!(mergiraf_applies(yes), "{yes}");
        }
        for no in [
            "README.md",
            ".changeset/x.md",
            "a.lock",
            "Makefile",
            "x.RS",
            "x.js",
        ] {
            assert!(!mergiraf_applies(no), "{no}");
        }
    }
}
