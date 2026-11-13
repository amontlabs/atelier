//! Promotion in mode A (r4 §3.8 8A, B8) and `atelier promote [--discard]`.
//!
//! The integration branch is checked out in the main checkout and is moved by one local push
//! into it, with `receive.denyCurrentBranch=updateInstead`, a lease on `OLD` and
//! `denyNonFastForwards`, so git itself updates the files and refuses an unclean checkout.
//! Immediately before each push atelier makes the five checks git does not make (a–e). A failed
//! check changes nothing, and neither does a refusal by git that left the main checkout exactly
//! as it was (judged by content, not by git's words), so only the promotion is repeated, for
//! `promotion_wait_seconds`, then exit 7 with the record kept.
//!
//! The push runs as its own process group, recorded under the gate lock, and is never signalled
//! or time-limited: atelier waits until its group is empty. A stop signal reaching atelier only
//! ends the retries. The promotion record says when the last push started and whether it ended
//! leaving nothing behind (`push_ended`). A push that started and did not end that way, with the
//! ref still at `OLD` and check d or e failing, is an interrupted promotion: `merge` and
//! `promote` refuse (4) and print, path by path, which content the main checkout holds. They
//! reset nothing.
//!
//! The three questions (r4 §3.8) settle a record found by a later command: `M` is in the
//! integration branch (the promotion happened: record it), the branch is still at `OLD`
//! (pending), or it moved elsewhere (the record is void: run the merge again).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::dispatch::DISPATCHED_VAR;
use crate::exit::{Exit, Failure, IoContext, Result};
use crate::fsutil;
use crate::git;
use crate::ledger::{Approval, MergeLine, Promotion};
use crate::locks::{HeldLock, LockKey};
use crate::names;
use crate::preflight::{CONVERTERS, Context};
use crate::procs;
use crate::records::{BranchState, MergeFacts};
use crate::refs;
use crate::runs::STEP_TRAMPOLINE;
use crate::signals;
use crate::stack::{Stack, is_ancestor, short};
use crate::state::{CURRENT_FORMAT, Versioned};

/// The pause between two tries; never a limit on one.
const RETRY_PAUSE: Duration = Duration::from_secs(2);

/// How long recording a branch as merged waits for its branch lock.
const BRANCH_LOCK_WAIT: Duration = Duration::from_secs(10);

/// The receiving side's settings, carried with the command so the repository's configuration
/// is neither needed nor changed [P3]. `core.hooksPath` names an empty folder atelier owns, so
/// none of the repository's hooks (`pre-receive`, `update`, `reference-transaction`,
/// `push-to-checkout`, …) runs: git's own test that the main checkout is clean decides.
pub fn receive_pack(empty_hooks: &Path) -> String {
    format!(
        "git -c receive.denyCurrentBranch=updateInstead -c receive.denyNonFastForwards=true -c core.hooksPath={} receive-pack",
        crate::remote::sq(&empty_hooks.to_string_lossy())
    )
}

/// `<state>/empty-hooks`: an empty folder, created when missing. Refuses (4) when something is
/// in it or it is not a real folder: atelier never empties it.
pub fn empty_hooks(state: &crate::state::State) -> Result<PathBuf> {
    let dir = state.root().join("empty-hooks");
    match fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            fs::create_dir(&dir).ctx(dir.display())?;
        }
        Err(e) => return Err(Failure::refused(format!("{}: {e}", dir.display()))),
        Ok(meta) if !meta.is_dir() => {
            return Err(Failure::refused(format!(
                "{} is not a folder: atelier keeps it as an empty hooks folder for the promotion's push; move it away",
                dir.display()
            )));
        }
        Ok(_) => {}
    }
    let inside: Vec<String> = fs::read_dir(&dir)
        .ctx(dir.display())?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    if !inside.is_empty() {
        return Err(Failure::refused(format!(
            "{} must stay empty (the promotion's push runs no hooks), and it holds: {}; remove them",
            dir.display(),
            inside.join(", ")
        )));
    }
    Ok(dir)
}

/// Variables that would point git at another repository; removed from the push's environment.
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

/// What git says about a promotion record (r4 §3.8, the three questions, in this order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Facts {
    /// `M` is the integration branch's commit or an ancestor of it: the promotion happened.
    Promoted,
    /// The integration branch is still at `OLD`: the promotion is pending.
    Pending,
    /// The branch moved to something that does not contain `M` (or is gone).
    Moved(Option<String>),
}

pub fn facts(root: &Path, rec: &Promotion) -> Result<Facts> {
    let tip = refs::read(root, &names::branch_ref(&rec.integration))?;
    let Some(tip) = tip else {
        return Ok(Facts::Moved(None));
    };
    if is_ancestor(root, &rec.m, &tip)? {
        return Ok(Facts::Promoted);
    }
    if tip == rec.old {
        return Ok(Facts::Pending);
    }
    Ok(Facts::Moved(Some(tip)))
}

/// The refusal for a record whose integration branch moved elsewhere; the record is discarded
/// by the caller.
fn moved(root: &Path, rec: &Promotion, to: &Option<String>) -> Failure {
    Failure::refused(format!(
        "{} moved: run the merge again ({} is at {}, not {} that the gate merged onto; the promotion of {} is discarded, the approvals stay)",
        rec.integration,
        rec.integration,
        to.as_deref()
            .map(|t| short(root, t))
            .unwrap_or_else(|| "nothing".to_owned()),
        short(root, &rec.old),
        short(root, &rec.m)
    ))
}

/// Checks a and b: the integration ref is plain and checked out in the main checkout or
/// nowhere; the repository has no `push-to-checkout` hook. A failure refuses (4) at once.
pub fn check_ab(root: &Path, integration: &str) -> Result<()> {
    let refname = names::branch_ref(integration);
    refs::read(root, &refname)?.ok_or_else(|| {
        Failure::refused(format!(
            "the integration branch {integration} does not exist"
        ))
    })?;
    let main = git::canonical(root);
    for w in git::worktree_list(root)? {
        if w.branch.as_deref() == Some(refname.as_str()) && git::canonical(&w.path) != main {
            return Err(Failure::refused(format!(
                "{integration} is checked out in {}: atelier moves it only in the main checkout ({}) or where nobody has it",
                w.path.display(),
                main.display()
            )));
        }
    }
    let hook = PathBuf::from(git::run(
        root,
        [
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "hooks/push-to-checkout",
        ],
    )?);
    if fs::symlink_metadata(&hook).is_ok() {
        return Err(Failure::refused(format!(
            "the repository has a push-to-checkout hook ({}): it would replace git's own test that the main checkout is clean; remove it to let atelier promote",
            hook.display()
        )));
    }
    Ok(())
}

/// Check c: no merge, rebase, cherry-pick, revert or bisect in progress in the main checkout,
/// no `index.lock`, and no rebase or bisect of the integration branch open in any worktree.
pub fn check_c(root: &Path, stack: &Stack, integration: &str) -> Result<Vec<String>> {
    let git_dir = PathBuf::from(git::run(
        root,
        ["rev-parse", "--path-format=absolute", "--git-dir"],
    )?);
    let mut out = Vec::new();
    for (file, what) in [
        ("MERGE_HEAD", "a merge"),
        ("rebase-merge", "a rebase"),
        ("rebase-apply", "a rebase or git am"),
        ("CHERRY_PICK_HEAD", "a cherry-pick"),
        ("REVERT_HEAD", "a revert"),
        ("sequencer", "a cherry-pick or revert sequence"),
        ("BISECT_LOG", "a bisect"),
        ("BISECT_START", "a bisect"),
    ] {
        if fs::symlink_metadata(git_dir.join(file)).is_ok() {
            let line = format!("{what} is in progress in the main checkout");
            if !out.contains(&line) {
                out.push(line);
            }
        }
    }
    if fs::symlink_metadata(git_dir.join("index.lock")).is_ok() {
        out.push(format!(
            "{} exists: a git command is running in the main checkout, or one died",
            git_dir.join("index.lock").display()
        ));
    }
    if let Some(wt) = stack.rebase_open(integration)? {
        out.push(format!(
            "a rebase of {integration} is open in {}",
            wt.display()
        ));
    }
    if let Some(wt) = stack.bisect_open(integration)? {
        out.push(format!(
            "a bisect of {integration} is open in {}",
            wt.display()
        ));
    }
    Ok(out)
}

/// The tracked paths `git status --porcelain=v2 --untracked-files=no` reports (check d).
pub fn tracked_changes(root: &Path) -> Result<Vec<String>> {
    let out = git::output(
        root,
        ["status", "--porcelain=v2", "-z", "--untracked-files=no"],
    )?;
    if !out.status.success() {
        return Err(Failure::refused(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let mut paths = Vec::new();
    let mut fields = out.stdout.split(|&b| b == 0).filter(|f| !f.is_empty());
    while let Some(f) = fields.next() {
        let text = String::from_utf8_lossy(f);
        let parts: Vec<&str> = text.splitn(11, ' ').collect();
        match parts.first().copied() {
            // 1 XY sub mH mI mW hH hI path
            Some("1") if parts.len() >= 9 => paths.push(parts[8..].join(" ")),
            // 2 XY sub mH mI mW hH hI Xscore path, then the original path
            Some("2") if parts.len() >= 10 => {
                paths.push(parts[9..].join(" "));
                fields.next();
            }
            // u XY sub m1 m2 m3 mW h1 h2 h3 path
            Some("u") if parts.len() >= 11 => paths.push(parts[10].to_owned()),
            _ => paths.push(text.into_owned()),
        }
    }
    Ok(paths)
}

/// The paths `git diff --name-only --no-renames --diff-filter=<filter> OLD M` names: `A` for
/// the ones `M` adds, `D` for the ones it deletes.
fn diff_paths(root: &Path, old: &str, m: &str, filter: &str) -> Result<Vec<String>> {
    let out = git::output(
        root,
        [
            "diff",
            "--name-only",
            "--no-renames",
            &format!("--diff-filter={filter}"),
            "-z",
            old,
            m,
            "--",
        ],
    )?;
    if !out.status.success() {
        return Err(Failure::refused(format!(
            "git diff {old} {m} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(split_z(&out.stdout))
}

fn split_z(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|&b| b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .collect()
}

/// True when the volume holding `root` ignores case: `.git` and `.GIT` are one entry.
fn ignores_case(root: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (
        fs::symlink_metadata(root.join(".git")),
        fs::symlink_metadata(root.join(".GIT")),
    ) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// The paths OLD tracks and `M` deletes, as the volume compares names: git removes them when it
/// checks `M` out, so what stands there is no obstruction.
struct Leaving {
    paths: BTreeSet<String>,
    fold: bool,
}

impl Leaving {
    fn key(&self, p: &str) -> String {
        if self.fold {
            p.to_lowercase()
        } else {
            p.to_owned()
        }
    }

    /// `rel` is one of them.
    fn has(&self, rel: &str) -> bool {
        self.paths.contains(&self.key(rel))
    }

    /// Everything inside the folder `dir` (at `rel`) is one of them, folders aside: the folder
    /// goes once git has removed them. The first other entry otherwise.
    fn holds_only_leaving(&self, dir: &Path, rel: &str) -> std::result::Result<(), String> {
        let entries = fs::read_dir(dir).map_err(|e| format!("{rel}: {e}"))?;
        for e in entries {
            let e = e.map_err(|e| format!("{rel}: {e}"))?;
            let name = e.file_name().to_string_lossy().into_owned();
            let child = format!("{rel}/{name}");
            let meta = fs::symlink_metadata(e.path()).map_err(|e| format!("{child}: {e}"))?;
            if meta.is_dir() {
                self.holds_only_leaving(&e.path(), &child)?;
            } else if !self.has(&child) {
                return Err(child);
            }
        }
        Ok(())
    }
}

/// Check e: nothing exists at a path `M` adds, and each of its parent folders that exists is a
/// real folder; except what git itself removes first: a path OLD tracks and `M` deletes (by the
/// volume's comparison of names, so a case-only rename on a volume that ignores case is none), or
/// a folder holding only such paths. Check d has made sure those hold OLD's content. Returns the
/// obstructions.
pub fn check_e(root: &Path, old: &str, m: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let added = diff_paths(root, old, m, "A")?;
    if added.is_empty() {
        return Ok(out);
    }
    let fold = ignores_case(root);
    let mut leaving = Leaving {
        paths: BTreeSet::new(),
        fold,
    };
    leaving.paths = diff_paths(root, old, m, "D")?
        .iter()
        .map(|p| leaving.key(p))
        .collect();
    for rel in added {
        let mut prefix = root.to_path_buf();
        let parts: Vec<&str> = rel.split('/').collect();
        for (i, part) in parts.iter().enumerate() {
            prefix.push(part);
            let last = i + 1 == parts.len();
            let at = parts[..=i].join("/");
            match fs::symlink_metadata(&prefix) {
                Ok(meta) => {
                    if !meta.is_dir() && leaving.has(&at) {
                        // OLD's own file, removed by git before it writes here.
                        break;
                    }
                    if last {
                        if meta.is_dir() {
                            match leaving.holds_only_leaving(&prefix, &at) {
                                Ok(()) => break,
                                Err(other) => out.push(format!(
                                    "{rel}: a folder is there, where the merge adds a file, holding {other}"
                                )),
                            }
                        } else {
                            out.push(format!(
                                "{rel}: {} is there, where the merge adds a file",
                                kind(&meta)
                            ));
                        }
                    } else if !meta.is_dir() {
                        out.push(format!(
                            "{rel}: {} is at {at}, where the merge needs a folder",
                            kind(&meta),
                        ));
                    } else {
                        continue;
                    }
                    break;
                }
                Err(e) if e.kind() == ErrorKind::NotFound => break,
                Err(e) => {
                    out.push(format!("{rel}: {e}"));
                    break;
                }
            }
        }
    }
    Ok(out)
}

fn kind(meta: &fs::Metadata) -> &'static str {
    let t = meta.file_type();
    if t.is_symlink() {
        "a symbolic link"
    } else if t.is_dir() {
        "a folder"
    } else {
        "a file"
    }
}

/// Checks c, d and e, in that order; their failures are "busy", waited for.
fn busy(root: &Path, stack: &Stack, rec: &Promotion) -> Result<Vec<String>> {
    let mut out = check_c(root, stack, &rec.integration)?;
    let changes = tracked_changes(root)?;
    if !changes.is_empty() {
        out.push(format!("main checkout has changes: {}", list(&changes)));
    }
    out.extend(check_e(root, &rec.old, &rec.m)?);
    Ok(out)
}

fn list(paths: &[String]) -> String {
    match paths {
        [] => String::new(),
        [one] => one.clone(),
        [first, rest @ ..] => format!("{first} (+{} more)", rest.len()),
    }
}

/// True when check d or e fails: what an interrupted push leaves.
fn disturbed(root: &Path, rec: &Promotion) -> Result<bool> {
    Ok(!tracked_changes(root)?.is_empty() || !check_e(root, &rec.old, &rec.m)?.is_empty())
}

/// How one push ended.
struct Pushed {
    /// git's own words.
    output: String,
    /// Ended by a signal (its group was killed).
    signalled: bool,
}

/// One push into the main checkout, to its end. The record says `push_started` before, and
/// `push_ended` after only when the push left nothing behind; its group is recorded under the
/// gate lock while it lives.
fn push(
    root: &Path,
    state: &crate::state::State,
    v: &Versioned,
    lock: &mut HeldLock,
    rec: &mut Promotion,
    log: &Path,
) -> Result<Pushed> {
    let hooks = empty_hooks(state)?;
    rec.push_started = Some(fsutil::rfc3339(fsutil::now_secs()));
    rec.push_pgid = None;
    rec.push_ended = None;
    rec.write(v)?;
    let refname = names::branch_ref(&rec.integration);
    let log_file = File::options()
        .create(true)
        .append(true)
        .open(log)
        .ctx(log.display())?;
    let log_err = log_file.try_clone().ctx(log.display())?;
    let mut cmd = Command::new("/bin/sh");
    cmd.args([
        "-c",
        STEP_TRAMPOLINE,
        "atelier-push",
        "git",
        "-c",
        "push.gpgSign=false",
        "push",
        "--porcelain",
        "--no-verify",
    ])
    .arg(format!("--receive-pack={}", receive_pack(&hooks)))
    .arg(format!("--force-with-lease={refname}:{}", rec.old))
    .arg(".")
    .arg(format!("{}:{refname}", rec.m))
    .current_dir(root)
    .stdin(Stdio::piped())
    .stdout(log_file)
    .stderr(log_err)
    .env_remove(DISPATCHED_VAR)
    .process_group(0);
    for var in REDIRECTING_VARS {
        cmd.env_remove(var);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| Failure::refused(format!("cannot start git push: {e}")))?;
    let pgid = child.id() as i32;
    let gate = child.stdin.take();
    let recorded = lock.add_group(pgid, &rec.run, procs::start_time(pgid));
    if let Err(e) = recorded {
        // Nothing ran yet: closing the pipe makes the trampoline exit without running git.
        drop(gate);
        let _ = child.wait();
        return Err(e);
    }
    rec.push_pgid = Some(pgid);
    let _ = rec.write(v);
    if let Some(mut g) = gate {
        let _ = g.write_all(b"go\n");
    }
    // No time limit and no signal: the push, and anything it left in its group (the step that
    // writes the files can outlive the processes above it), runs to its end.
    loop {
        match procs::exited(&child) {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
        sleep(Duration::from_millis(20));
    }
    while procs::group_occupied(pgid) {
        sleep(Duration::from_millis(20));
    }
    let status = child.wait().ok();
    lock.remove_group(pgid)?;
    let output = fs::read_to_string(log).unwrap_or_default();
    Ok(Pushed {
        output,
        signalled: status.is_some_and(|s| s.signal().is_some()),
    })
}

/// True when the main checkout shows that a push that left the ref at `OLD` wrote nothing, by
/// content, whatever git said: every path that differs between `OLD` and `M`, and every other
/// tracked path `git status` reports, holds `OLD`'s content ([`classify`]; so none of `M`'s
/// added paths is there), and the index is `OLD`'s tree. git can refuse the ref after it wrote
/// files (`updateInstead` runs `read-tree -u -m` before it moves the ref, and reports a
/// failure there as a refusal), so its words cannot tell.
fn wrote_nothing(root: &Path, rec: &Promotion) -> Result<bool> {
    if classify(root, &rec.old, &rec.m)?
        .iter()
        .any(|c| c.held != Held::Old)
    {
        return Ok(false);
    }
    let index = git::output(root, ["diff-index", "--cached", "--quiet", &rec.old, "--"])?;
    Ok(index.status.success())
}

/// git's reason for refusing a push, from its output: the lines that say why.
fn git_reason(output: &str) -> String {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|l| {
            l.starts_with("! ")
                || l.starts_with("error:")
                || l.starts_with("fatal:")
                || l.starts_with("remote: error:")
                || l.starts_with("remote: fatal:")
                || l.contains("rejected")
        })
        .collect();
    let picked = if lines.is_empty() {
        output.lines().last().unwrap_or("").trim().to_owned()
    } else {
        lines.join("; ")
    };
    format!("git refused the push: {picked}")
}

/// Who runs the promotion: the word that starts the exit-7 line and the folder for push logs.
pub struct Caller<'a> {
    pub word: &'a str,
    /// The folder the push log and the path classification go to.
    pub log_dir: PathBuf,
}

/// Step 8A with the wait, then step 9. Holds the gate lock throughout.
pub fn promote(
    ctx: &Context,
    v: &Versioned,
    lock: &mut HeldLock,
    mut rec: Promotion,
    caller: &Caller,
) -> Result<Exit> {
    let root = ctx.checkout.root.clone();
    let stack = Stack::new(&ctx.checkout, &ctx.state, &rec.integration);
    let _signals = signals::install()
        .map_err(|e| Failure::refused(format!("cannot install signal handlers: {e}")))?;
    let wait = Duration::from_secs(ctx.settings.promotion_wait_seconds);
    let started = Instant::now();
    let mut tries = 0u32;
    loop {
        match facts(&root, &rec)? {
            Facts::Promoted => {
                if tries == 0 {
                    println!(
                        "the promotion of {} (run {}) happened; recording it",
                        short(&root, &rec.m),
                        rec.run
                    );
                }
                return finish(ctx, v, &rec);
            }
            Facts::Moved(to) => {
                Promotion::remove(v)?;
                return Err(moved(&root, &rec, &to));
            }
            Facts::Pending => {}
        }
        if interrupted(&root, &rec)? {
            return Err(interrupted_report(&root, &rec, caller)?);
        }
        check_ab(&root, &rec.integration)?;
        let mut reasons = busy(&root, &stack, &rec)?;
        if reasons.is_empty() {
            tries += 1;
            let log = caller.log_dir.join(format!("push-{tries}.log"));
            let pushed = push(&root, &ctx.state, v, lock, &mut rec, &log)?;
            match facts(&root, &rec)? {
                Facts::Promoted => {
                    rec.push_ended = Some(fsutil::rfc3339(fsutil::now_secs()));
                    rec.write(v)?;
                    return finish(ctx, v, &rec);
                }
                Facts::Moved(to) => {
                    Promotion::remove(v)?;
                    return Err(moved(&root, &rec, &to));
                }
                Facts::Pending => {}
            }
            // Checks d and e passed just before the push. A push that ended with the checkout
            // exactly as before (by content: `wrote_nothing`) is a clean refusal. Anything
            // else, a push killed or one that changed the checkout, did not end cleanly: the
            // record keeps `push_started` without `push_ended`, and the report below applies.
            let refused = !pushed.signalled && wrote_nothing(&root, &rec)?;
            if !refused && (pushed.signalled || disturbed(&root, &rec)?) {
                return Err(interrupted_report(&root, &rec, caller)?);
            }
            rec.push_ended = Some(fsutil::rfc3339(fsutil::now_secs()));
            rec.write(v)?;
            reasons.push(git_reason(&pushed.output));
        }
        let stop = signals::received();
        if stop.is_some() || started.elapsed() >= wait {
            let why = match stop {
                Some(sig) => format!("{} ended the wait; ", signals::name(sig)),
                None => String::new(),
            };
            println!(
                "{}: GREEN, NOT PROMOTED — run {}; {why}{}; run `atelier promote` when it is clean",
                caller.word,
                rec.run,
                reasons.join("; ")
            );
            return Ok(Exit::NotPromoted);
        }
        let until = Instant::now() + RETRY_PAUSE;
        while Instant::now() < until && signals::received().is_none() {
            sleep(Duration::from_millis(50));
        }
    }
}

/// An interrupted promotion (r4 §3.8 8A): a push started and did not end cleanly, the ref is
/// still at `OLD`, and check d or e fails.
pub fn interrupted(root: &Path, rec: &Promotion) -> Result<bool> {
    // No recorded group: atelier ended before releasing the push, so git never ran.
    if !rec.push_unfinished() || rec.push_pgid.is_none() {
        return Ok(false);
    }
    let tip = refs::read(root, &names::branch_ref(&rec.integration))?;
    if tip.as_deref() != Some(rec.old.as_str()) {
        return Ok(false);
    }
    disturbed(root, rec)
}

/// What a path in the main checkout holds, by content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    /// `OLD`'s content (absent where `OLD` has no such path): not yet written.
    Old,
    /// `M`'s content (absent where `M` deletes it): written.
    M,
    /// Something else: an empty file the interruption left, or the owner's.
    Other,
}

/// One path of the classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    pub path: String,
    pub held: Held,
    /// What is there: `absent`, `a folder`, or the size in bytes.
    pub what: String,
}

/// Classifies, by reading files only, every path that differs between `OLD` and `M` and every
/// other tracked path `git status` reports as changed.
pub fn classify(root: &Path, old: &str, m: &str) -> Result<Vec<Classified>> {
    let out = git::output(
        root,
        ["diff", "--name-only", "--no-renames", "-z", old, m, "--"],
    )?;
    let mut paths: BTreeSet<String> = split_z(&out.stdout).into_iter().collect();
    paths.extend(tracked_changes(root)?);
    let old_blobs = blobs(root, old)?;
    let m_blobs = blobs(root, m)?;

    // Current content: the blob id git would store for what is there.
    let mut now: BTreeMap<String, (Option<String>, String)> = BTreeMap::new();
    let mut regular = Vec::new();
    for p in &paths {
        let full = root.join(p);
        match fs::symlink_metadata(&full) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                now.insert(p.clone(), (None, "absent".to_owned()));
            }
            Err(e) => {
                now.insert(p.clone(), (Some(String::new()), format!("unreadable: {e}")));
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = fs::read_link(&full).ctx(full.display())?;
                let oid = hash_bytes(root, target.as_os_str().as_encoded_bytes())?;
                now.insert(p.clone(), (Some(oid), "a symbolic link".to_owned()));
            }
            Ok(meta) if meta.is_dir() => {
                now.insert(p.clone(), (Some(String::new()), "a folder".to_owned()));
            }
            Ok(meta) => {
                regular.push(p.clone());
                now.insert(p.clone(), (None, format!("{} bytes", meta.len())));
            }
        }
    }
    if !regular.is_empty() {
        let mut child = git::command(root)
            .args(["hash-object", "--stdin-paths"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Failure::refused(format!("cannot run git: {e}")))?;
        {
            let mut stdin = child.stdin.take().expect("piped");
            for p in &regular {
                stdin
                    .write_all(format!("{p}\n").as_bytes())
                    .ctx("git hash-object")?;
            }
        }
        let out = child.wait_with_output().ctx("git hash-object")?;
        if !out.status.success() {
            return Err(Failure::refused(format!(
                "git hash-object failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        for (p, oid) in regular.iter().zip(text.lines()) {
            if let Some(e) = now.get_mut(p) {
                e.0 = Some(oid.to_owned());
            }
        }
    }
    let mut result = Vec::new();
    for p in paths {
        let (oid, what) = now
            .remove(&p)
            .unwrap_or((Some(String::new()), String::new()));
        let held = if oid.as_ref() == old_blobs.get(&p) {
            Held::Old
        } else if oid.as_ref() == m_blobs.get(&p) {
            Held::M
        } else {
            Held::Other
        };
        result.push(Classified {
            path: p,
            held,
            what,
        });
    }
    Ok(result)
}

/// Path → blob id of every file of a commit.
fn blobs(root: &Path, commit: &str) -> Result<BTreeMap<String, String>> {
    let out = git::output(root, ["ls-tree", "-r", "-z", "--full-tree", commit])?;
    if !out.status.success() {
        return Err(Failure::refused(format!(
            "git ls-tree {commit} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let mut map = BTreeMap::new();
    for entry in split_z(&out.stdout) {
        // <mode> SP <type> SP <oid> TAB <path>
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        if let Some(oid) = meta.split(' ').nth(2) {
            map.insert(path.to_owned(), oid.to_owned());
        }
    }
    Ok(map)
}

fn hash_bytes(root: &Path, bytes: &[u8]) -> Result<String> {
    let mut child = git::command(root)
        .args(["hash-object", "--stdin", "--no-filters"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| Failure::refused(format!("cannot run git: {e}")))?;
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(bytes)
        .ctx("git hash-object")?;
    let out = child.wait_with_output().ctx("git hash-object")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The refusal (4) for an interrupted promotion: git's status, the classification counts, every
/// path of the third kind with what is there, the file with the whole classification, and the
/// routes. atelier runs none of them.
fn interrupted_report(root: &Path, rec: &Promotion, caller: &Caller) -> Result<Failure> {
    let classes = classify(root, &rec.old, &rec.m)?;
    let file = caller.log_dir.join("promotion-paths.tsv");
    let mut tsv = String::from("path\tholds\twhat\n");
    for c in &classes {
        tsv.push_str(&format!(
            "{}\t{}\t{}\n",
            c.path,
            match c.held {
                Held::Old => "old",
                Held::M => "m",
                Held::Other => "other",
            },
            c.what
        ));
    }
    fs::create_dir_all(&caller.log_dir).ctx(caller.log_dir.display())?;
    fsutil::write_atomic(&file, tsv.as_bytes()).ctx(file.display())?;
    let status = git::output(root, ["status", "--short", "--untracked-files=all"])
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let n_old = classes.iter().filter(|c| c.held == Held::Old).count();
    let n_m = classes.iter().filter(|c| c.held == Held::M).count();
    let other: Vec<&Classified> = classes.iter().filter(|c| c.held == Held::Other).collect();
    let (old, m) = (short(root, &rec.old), short(root, &rec.m));
    let mut msg = format!(
        "a promotion was interrupted at {}; the main checkout may hold part of {m}:\n\
         tracked files deleted or modified, new files that are untracked, and possibly an empty file\n\
         git status:\n{}",
        rec.push_started.as_deref().unwrap_or("an unknown time"),
        indent(&status)
    );
    msg.push_str(&format!(
        "paths holding {old}'s content (not yet written): {n_old}\n\
         paths holding {m}'s content (written): {n_m}\n"
    ));
    if other.is_empty() {
        msg.push_str("paths holding something else: none\n");
    } else {
        msg.push_str(&format!(
            "paths holding something else ({}; atelier cannot tell what the interruption left from your own files):\n",
            other.len()
        ));
        for c in &other {
            msg.push_str(&format!("  {} ({})\n", c.path, c.what));
        }
    }
    msg.push_str(&format!("every path, classified: {}\n", file.display()));
    if !other.is_empty() {
        msg.push_str(
            "Look at each path holding something else and remove or move it; then, as below.\n",
        );
    }
    msg.push_str(&format!(
        "When nothing holds something else, either reset loses nothing:\n\
         \x20 git reset --hard {} completes the promotion; then atelier promote records it;\n\
         \x20 or git reset --hard {}, remove the untracked files marked `m` in the file above, then atelier promote repeats the push.\n\
         atelier runs neither. atelier promote --discard drops the promotion without touching the checkout.",
        rec.m, rec.old
    ));
    Ok(Failure::refused(msg))
}

fn indent(text: &str) -> String {
    let mut out = String::new();
    for l in text.lines() {
        out.push_str("  ");
        out.push_str(l);
        out.push('\n');
    }
    if out.is_empty() {
        out.push_str("  (nothing)\n");
    }
    out
}

/// Step 9: one `merges.tsv` line per branch (once); each branch's record marked merged and its
/// approval removed; the lines printed (`merged <branch> <commit>`); then the promotion record
/// removed. Idempotent: a crash anywhere is finished by the next `promote` or `merge`, which
/// prints the lines again.
///
/// A branch that moved past its approved commit during the gate, or was approved again at
/// another commit, has newer commits that are not merged: its approved commit is merged
/// (`merges.tsv`, the printed line), but its record stays `active`, without merge facts, so the
/// branch stays usable (`follow` takes the newer commits onto the integration branch) and
/// `clean` deletes nothing of it. The newer approval stays.
///
/// Each record is written under the branch lock taken shared (as `tmp` and `check` hold it):
/// only `start`, `follow` and `clean`, which hold it exclusive, make it wait; after
/// [`BRANCH_LOCK_WAIT`] the record is kept and the next `promote` or `merge` finishes.
pub fn finish(ctx: &Context, v: &Versioned, rec: &Promotion) -> Result<Exit> {
    let root = &ctx.checkout.root;
    let stack = Stack::new(&ctx.checkout, &ctx.state, &rec.integration);
    let existing = MergeLine::read_all(v)?;
    for b in &rec.branches {
        let done = existing
            .iter()
            .any(|l| l.branch == b.name && l.lifetime == b.lifetime && l.m == rec.m);
        if !done {
            MergeLine::new(rec, b).append(v)?;
        }
    }
    let mut newer: Vec<String> = Vec::new();
    for b in &rec.branches {
        let lock = take_branch_lock(&ctx.state, &b.name).map_err(|e| {
            Failure::refused(format!(
                "{} is promoted to {}; recording {} as merged waits for its lock ({}): run atelier promote to finish",
                rec.integration,
                short(root, &rec.m),
                b.name,
                e.message
            ))
        })?;
        let tip = refs::read(root, &names::branch_ref(&b.name))?;
        let approval = Approval::read(v, &b.name)?;
        let reapproved = approval
            .as_ref()
            .is_some_and(|a| a.lifetime == b.lifetime && a.commit != b.commit);
        let moved = tip.as_deref().is_some_and(|t| t != b.commit);
        if let Some(r) = stack.record(&b.name)?
            && r.lifetime == b.lifetime
            && r.state == BranchState::Active
        {
            if moved || reapproved {
                let at = tip.as_deref().unwrap_or(&b.commit);
                newer.push(format!(
                    "{} moved to {} after its approval at {}: its later commits are not merged; it stays active (atelier follow in its worktree, then approve it again)",
                    b.name,
                    short(root, at),
                    short(root, &b.commit)
                ));
            } else {
                MergeFacts {
                    commit: b.commit.clone(),
                    worktree: b.worktree.clone(),
                    dev: b.dev,
                    ino: b.ino,
                    later: toml::Table::new(),
                }
                .record(&ctx.state, &b.name, &b.lifetime)?;
            }
        }
        if let Some(a) = approval
            && a.lifetime == b.lifetime
            && a.commit == b.commit
        {
            Approval::remove(v, &b.name)?;
        }
        drop(lock);
    }
    if let Some(line) = &rec.summary {
        println!("{line}");
    }
    for b in &rec.branches {
        println!("merged {} {}", b.name, short(root, &b.commit));
    }
    for line in &newer {
        println!("{line}");
    }
    if rec.rules_changed {
        println!("rules changed");
    }
    Promotion::remove(v)?;
    Ok(Exit::Ok)
}

fn take_branch_lock(state: &crate::state::State, branch: &str) -> Result<HeldLock> {
    let deadline = Instant::now() + BRANCH_LOCK_WAIT;
    loop {
        match HeldLock::acquire_shared(state, LockKey::branch(branch)) {
            Ok(l) => return Ok(l),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => sleep(Duration::from_millis(200)),
        }
    }
}

/// A promotion record found before a new gate (by `merge`) or by `promote`: settled by the
/// three questions. `Ok(true)`: nothing is pending any more (recorded or discarded), the caller
/// may go on; refusals (4) for a pending or interrupted promotion.
pub fn settle_found(ctx: &Context, v: &Versioned, rec: &Promotion, log_dir: &Path) -> Result<()> {
    let root = &ctx.checkout.root;
    match facts(root, rec)? {
        Facts::Promoted => {
            println!(
                "an earlier promotion of {} (run {}) happened; recording it",
                short(root, &rec.m),
                rec.run
            );
            finish(ctx, v, rec)?;
            Ok(())
        }
        Facts::Moved(to) => {
            Promotion::remove(v)?;
            eprintln!("atelier: {}", moved(root, rec, &to).message);
            Ok(())
        }
        Facts::Pending => {
            if interrupted(root, rec)? {
                return Err(interrupted_report(
                    root,
                    rec,
                    &Caller {
                        word: "merge",
                        log_dir: log_dir.to_path_buf(),
                    },
                )?);
            }
            Err(Failure::refused(format!(
                "a promotion is pending (run {}, {} {} → {}): run atelier promote, or atelier promote --discard",
                rec.run,
                rec.integration,
                short(root, &rec.old),
                short(root, &rec.m)
            )))
        }
    }
}

/// `atelier promote [--discard]`.
pub fn run(ctx: &Context, discard: bool) -> Result<Exit> {
    let v = ctx.state.versioned(CURRENT_FORMAT, CONVERTERS)?;
    let mut lock = HeldLock::acquire(&ctx.state, LockKey::Gate)?;
    let root = &ctx.checkout.root;
    let Some(rec) = Promotion::read(&v)? else {
        return Err(Failure::refused("no promotion is pending"));
    };
    let log_dir = ctx.state.runs_dir().join(&rec.run);
    if discard {
        if facts(root, &rec)? == Facts::Promoted {
            return Err(Failure::refused(format!(
                "the promotion of {} happened ({} contains it): atelier promote records it",
                short(root, &rec.m),
                rec.integration
            )));
        }
        Promotion::remove(&v)?;
        println!(
            "promotion of {} (run {}) discarded; the main checkout is untouched and the approvals stay",
            short(root, &rec.m),
            rec.run
        );
        return Ok(Exit::Ok);
    }
    let caller = Caller {
        word: "promote",
        log_dir,
    };
    let exit = promote(ctx, &v, &mut lock, rec, &caller)?;
    if exit == Exit::Ok {
        println!("promote: done");
    }
    Ok(exit)
}
