//! `atelier follow` (DESIGN §2.4, §8.2): rebases exactly this branch's own commits onto what it
//! is built on, when that moved.
//!
//! In order: refuse a rebase of the branch open in any worktree and settle an earlier pending
//! follow (from ancestry alone); refuse uncommitted changes; refuse a branch that no longer
//! contains its base (no automatic repair); refuse a merge commit of the branch's own; pick the
//! target (the lower branch's tip, or the integration branch once the lower is merged, or gone
//! with its base in the integration branch), refusing a lower branch with a rebase open; record
//! *pending*; `git rebase --onto <target> <base>`, never with an autostash, as the one step of a
//! `follow` run under the branch's lock, so its process group is recorded and a later command
//! refuses while it lives. A conflict leaves the rebase open and exits 5 with the files. With
//! `[merge] resolver = "mergiraf"`, an ordinary three-way content conflict on a regular `.rs`,
//! `.ts`, `.tsx`, `.toml` or `.json` file, whose three stages are that path's file in the
//! commits being merged (no rename), is first given to `mergiraf solve --stdout`; atelier
//! writes a result with no conflict left itself, through the folder's descriptor, stages it
//! in the open rebase and prints it. Any other conflict is left as git's.
//!
//! The outcome is the settled record's: a rebase that finished is exit 0 even when a hook left
//! processes behind, which are stopped and named in a warning.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::dirfd::Parent;
use crate::exit::{Exit, Failure, Result};
use crate::git::{self, Checkout};
use crate::locks::{HeldLock, LockKey};
use crate::preflight::Context;
use crate::records::Pending;
use crate::refs;
use crate::runs::{Run, RunKind, StepStatus};
use crate::stack::{FINISH_REBASE, Stack, Target, is_ancestor, short};

/// The only files `mergiraf solve` is run on (DESIGN §8.2, P21).
pub const MERGIRAF_EXTENSIONS: &[&str] = &["rs", "ts", "tsx", "toml", "json"];

/// The resolver, found on `PATH`.
const MERGIRAF: &str = "mergiraf";

/// The rebase step's name in the run (its log is `rebase.log`).
const REBASE_STEP: &str = "rebase";

/// Variables that would point the rebase at another repository; removed from its environment.
const REDIRECTING_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

pub fn run(ctx: &Context) -> Result<Exit> {
    let root = &ctx.checkout.root;
    let branch = current_branch(&ctx.checkout)?;
    let integration = &ctx.settings.integration_branch;
    let lock = HeldLock::acquire(&ctx.state, LockKey::branch(&branch))?;
    let stack = Stack::new(&ctx.checkout, &ctx.state, integration);
    let resolver = ctx.settings.merge.resolver.as_deref() == Some("mergiraf");
    if resolver {
        check_mergiraf()?;
    }

    let (mut rec, tip) = stack.prepare(&branch)?;

    let changes = uncommitted(root)?;
    if !changes.is_empty() {
        return Err(Failure::refused(format!(
            "commit first: uncommitted changes in {}",
            list(&changes)
        )));
    }

    let target = stack.target(&rec)?;
    if !target.repoint
        && !rec.lower_is_integration
        && let Some(wt) = stack.rebase_open(&rec.lower)?
    {
        return Err(Failure::refused(format!(
            "{} has a rebase open in {}: follow once it is finished",
            rec.lower,
            wt.display()
        )));
    }
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
            "follow: {branch} {} on {} ({})",
            if moved { "now" } else { "already" },
            short(root, &target.commit),
            commits(root, &target.commit, &tip)?
        );
        return Ok(Exit::Ok);
    }

    rec.pending = Some(Pending {
        target: target.commit.clone(),
        old_base: rec.base.clone(),
        later: toml::Table::new(),
    });
    rec.write(&ctx.state)?;

    // From here the branch lock belongs to the run: the rebase's process group is recorded
    // under it before git starts, and stays recorded if atelier dies while git runs.
    let mut run = Run::start_held(&ctx.state, lock, RunKind::Follow, "atelier follow", root)?;
    run.set_branch(Some(branch.clone()));
    let outcome = rebase(&mut run, &stack, root, &branch, rec, &target, resolver);
    let passed = matches!(outcome, Ok(Exit::Ok));
    let result = run.finish(passed)?;
    for step in &result.steps {
        if !step.left.is_empty() {
            eprintln!(
                "follow: warning: the rebase (its hooks) left processes behind, which atelier stopped: {}",
                step.left.join("; ")
            );
        }
        if !step.unstoppable.is_empty() {
            eprintln!(
                "follow: the rebase left processes that could not be stopped: {}",
                step.unstoppable.join("; ")
            );
        }
    }
    if !result.survivors.is_empty() {
        eprintln!(
            "follow: processes of the rebase are still running (the next command on {branch} waits for them): {}",
            result.survivors.join("; ")
        );
    }
    outcome
}

/// Runs the rebase as the run's step and settles what it left: exit 0 with the new tip, or the
/// exit-5 conflict with the files.
fn rebase(
    run: &mut Run,
    stack: &Stack,
    root: &Path,
    branch: &str,
    rec: crate::records::BranchRecord,
    target: &Target,
    resolver: bool,
) -> Result<Exit> {
    let mut argv: Vec<String> = vec!["env".into()];
    for var in REDIRECTING_VARS {
        argv.extend(["-u".into(), (*var).into()]);
    }
    argv.push("git".into());
    for c in [
        "rebase.autoStash=false",
        "rebase.updateRefs=false",
        "rebase.autoSquash=false",
        "gc.auto=0",
        "maintenance.auto=false",
    ] {
        argv.extend(["-c".into(), c.into()]);
    }
    if resolver {
        argv.extend(["-c".into(), "merge.conflictStyle=diff3".into()]);
    }
    argv.extend(
        [
            "rebase",
            "--no-autostash",
            "--no-update-refs",
            "--onto",
            &target.commit,
            &rec.base,
        ]
        .map(String::from),
    );
    let env = [("GIT_EDITOR", ":"), ("GIT_SEQUENCE_EDITOR", ":")]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
    let step = run.step(REBASE_STEP, &argv, root, &env)?;
    let log_path = run.dir().join(&step.log);
    let log = fs::read_to_string(&log_path).unwrap_or_default();
    if step.status == StepStatus::Interrupted {
        return Err(Failure::refused(format!(
            "follow was interrupted during the rebase of {branch}; run atelier follow again (git's output: {})",
            log_path.display()
        )));
    }

    if stack.rebase_open(branch)?.is_some() {
        return Err(conflict(root, branch, &target.commit, resolver, &log)?);
    }
    let new_tip = stack
        .tip(branch)?
        .ok_or_else(|| Failure::refused(format!("{branch} disappeared during the rebase")))?;
    // Judged by the settled record, not by the step's status: a rebase that finished is
    // finished even when a hook left a process behind (`LeftProcesses`).
    let rec = stack.settle(rec, &new_tip)?;
    if rec.base != target.commit {
        return Err(Failure::refused(format!(
            "git rebase {}: {}",
            if step.status == StepStatus::NotStarted {
                "did not start"
            } else {
                "did not finish"
            },
            log.trim()
        )));
    }
    println!(
        "follow: {branch} now on {} ({})",
        short(root, &rec.base),
        commits(root, &rec.base, &new_tip)?
    );
    Ok(Exit::Ok)
}

/// The branch `HEAD` names, read without following a symbolic branch ref: a branch that is an
/// alias of another is refused (4), not resolved (3.3 review 2). Refuses (4) a detached
/// `HEAD`, and an open rebase.
fn current_branch(checkout: &Checkout) -> Result<String> {
    let out = git::output(
        &checkout.root,
        ["symbolic-ref", "--no-recurse", "-q", "HEAD"],
    )?;
    if out.status.success() {
        let r = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if let Some(b) = r.strip_prefix("refs/heads/") {
            refs::ensure_plain(&checkout.root, &r)?;
            return Ok(b.to_owned());
        }
        return Err(Failure::refused(format!(
            "HEAD names {r}, not a branch: follow runs on a branch atelier started"
        )));
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
    for (f, stages) in &files {
        if resolver
            && mergiraf_applies(f)
            && content_conflict(stages)
            && same_path_in_each_side(root, f, stages)?
            && mergiraf_solve(root, f)?
        {
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

/// True for an ordinary three-way content conflict on a regular file: the base (stage 1) and
/// both sides (2, 3) are present and each is a regular file (mode 100644 or 100755). A
/// modify/delete, an add/add without base, a rename that left a side missing, a symbolic link
/// or a submodule stays git's conflict (3.3 review 1, 4).
fn content_conflict(stages: &BTreeMap<u8, Stage>) -> bool {
    [1, 2, 3].iter().all(|n| {
        stages
            .get(n)
            .is_some_and(|s| s.mode == "100644" || s.mode == "100755")
    }) && stages.len() == 3
}

/// True when each stage of `rel` is the file at that same path in the commit it comes from:
/// stage 1 in the parent of the commit being replayed (`REBASE_HEAD^`, the merge base), 2 in
/// `HEAD`, 3 in `REBASE_HEAD`. False when a side renamed the file (the path is missing from the
/// merge base, or holds another file there), or when the commits cannot be read: Mergiraf's
/// last resort reads `<commit>:<path>` for each side, which then names the wrong file.
fn same_path_in_each_side(root: &Path, rel: &str, stages: &BTreeMap<u8, Stage>) -> Result<bool> {
    let revs = ["REBASE_HEAD^1", "HEAD", "REBASE_HEAD"].map(|c| format!("{c}:{rel}"));
    let out = git::output(
        root,
        ["rev-parse"]
            .into_iter()
            .chain(revs.iter().map(String::as_str)),
    )?;
    if !out.status.success() {
        return Ok(false);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let found: Vec<&str> = text.lines().collect();
    Ok(found.len() == 3
        && (1..=3u8)
            .zip(&found)
            .all(|(n, oid)| stages.get(&n).is_some_and(|s| s.object == *oid)))
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

/// Runs `mergiraf solve --stdout` on one conflicted regular file, so Mergiraf never writes.
/// When it solves every conflict (exit 0) and its output has no conflict marker, atelier
/// replaces the file by that output with the file's own permission bits, through the
/// descriptor of the file's folder (no link followed on the way, the folders checked to still
/// be the same before and after), stages it and returns true. Otherwise the file is left as
/// git wrote it, or put back if it was replaced and staging failed.
fn mergiraf_solve(root: &Path, rel: &str) -> Result<bool> {
    let Some(parent) = Parent::open(root, rel) else {
        return Ok(false);
    };
    let Some(original) = parent.read() else {
        return Ok(false);
    };
    if !parent.still_there() {
        return Ok(false);
    }
    let out = Command::new(MERGIRAF)
        .args(["solve", "--stdout", "--"])
        .arg(rel)
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else {
        return Ok(false);
    };
    // The file Mergiraf read must still be git's, unchanged, at the same place.
    let unchanged = parent
        .read()
        .is_some_and(|now| now.identity == original.identity && now.contents == original.contents);
    if !out.status.success() || has_markers(&out.stdout) || !unchanged || !parent.still_there() {
        return Ok(false);
    }
    parent
        .replace(&out.stdout, original.mode)
        .map_err(|e| Failure::refused(format!("{rel}: {e}")))?;
    if parent.still_there() {
        let added = git::output(root, ["--literal-pathspecs", "add", "--", rel])?;
        if added.status.success() {
            return Ok(true);
        }
    }
    restore(&parent, rel, &original)?;
    Ok(false)
}

/// Puts git's conflicted file back, with its own permission bits, as a new regular file renamed
/// over the name in its folder, so nothing is written through a link. Refuses (4) when a folder
/// on the way is no longer the same real folder.
fn restore(parent: &Parent, rel: &str, original: &crate::dirfd::Snapshot) -> Result<()> {
    if !parent.still_there() {
        return Err(Failure::refused(format!(
            "cannot put git's conflict back in {rel}: a folder on its path is no longer the same real folder"
        )));
    }
    parent
        .replace(&original.contents, original.mode)
        .map_err(|e| Failure::refused(format!("{rel}: {e}")))
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

/// One unmerged stage of a path: its index mode and object.
struct Stage {
    mode: String,
    object: String,
}

/// The unmerged paths, relative to the worktree root, each with every stage present (1 base,
/// 2 ours, 3 theirs), from `git ls-files -u`.
fn conflicted(root: &Path) -> Result<Vec<(String, BTreeMap<u8, Stage>)>> {
    let out = git::output(root, ["ls-files", "-u", "-z", "--full-name"])?;
    if !out.status.success() {
        return Err(Failure::refused(format!(
            "git ls-files -u failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let mut by_path: BTreeMap<String, BTreeMap<u8, Stage>> = BTreeMap::new();
    for entry in out.stdout.split(|&b| b == 0).filter(|e| !e.is_empty()) {
        // `<mode> <object> <stage>\t<path>`
        let Some(tab) = entry.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let meta = String::from_utf8_lossy(&entry[..tab]);
        let path = String::from_utf8_lossy(&entry[tab + 1..]).into_owned();
        let mut fields = meta.split(' ');
        let (Some(mode), Some(object), Some(stage)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Ok(stage) = stage.parse::<u8>() else {
            continue;
        };
        by_path.entry(path).or_default().insert(
            stage,
            Stage {
                mode: mode.to_owned(),
                object: object.to_owned(),
            },
        );
    }
    Ok(by_path.into_iter().collect())
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

/// "1 commit" or "<n> commits": the branch's own commits above `from`.
fn commits(root: &Path, from: &str, to: &str) -> Result<String> {
    let n = git::run(root, ["rev-list", "--count", &format!("{from}..{to}")])?;
    Ok(if n == "1" {
        "1 commit".to_owned()
    } else {
        format!("{n} commits")
    })
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
