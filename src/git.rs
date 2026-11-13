//! Running git. Every call names its folder and drops the environment variables that would make
//! git act on another repository, index or ref namespace than the checkout atelier runs in.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::exit::{Failure, Result};

/// Variables that redirect git away from the folder it is started in.
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

static READ_ONLY: AtomicBool = AtomicBool::new(false);

/// Makes every git command this process starts leave the repository as it is: `git status` and
/// friends then never refresh the index (`GIT_OPTIONAL_LOCKS=0`). `status` turns it on.
pub fn set_read_only() {
    READ_ONLY.store(true, Ordering::Relaxed);
}

/// A git command in a given folder.
pub fn command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).stdin(Stdio::null());
    if READ_ONLY.load(Ordering::Relaxed) {
        cmd.env("GIT_OPTIONAL_LOCKS", "0");
    }
    for var in REDIRECTING_VARS {
        cmd.env_remove(var);
    }
    cmd
}

/// A git command that leads its own process group, so a Ctrl-C or a stop signal sent to the
/// terminal's group never reaches it (for a push that must not be cut short).
pub fn command_own_group(dir: &Path) -> Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = command(dir);
    cmd.process_group(0);
    cmd
}

/// Runs git and returns its output whatever the exit status.
pub fn output<I, S>(dir: &Path, args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    command(dir)
        .args(args)
        .output()
        .map_err(|e| Failure::refused(format!("cannot run git: {e}")))
}

/// Runs git, requires success, and returns stdout without its final newline.
pub fn run<I, S>(dir: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<_> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    let out = output(dir, &args)?;
    if !out.status.success() {
        return Err(Failure::refused(format!(
            "git {} failed: {}",
            args.iter()
                .map(|a| a.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    while s.ends_with('\n') {
        s.pop();
    }
    Ok(s)
}

/// The repository facts every command needs, read once.
#[derive(Debug, Clone)]
pub struct Checkout {
    /// The top folder of the worktree atelier runs in.
    pub root: PathBuf,
    /// This worktree's own git folder (`.git` or `.git/worktrees/<name>`).
    pub git_dir: PathBuf,
    /// The git folder shared by every worktree of the clone.
    pub common_dir: PathBuf,
}

impl Checkout {
    /// Finds the worktree containing `dir`. `None` outside a git worktree (including a bare
    /// repository or the inside of `.git`).
    pub fn find(dir: &Path) -> Result<Option<Checkout>> {
        let out = output(
            dir,
            [
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--absolute-git-dir",
                "--git-common-dir",
            ],
        )?;
        if !out.status.success() {
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() != 3 || lines[0].is_empty() {
            return Ok(None);
        }
        Ok(Some(Checkout {
            root: PathBuf::from(lines[0]),
            git_dir: PathBuf::from(lines[1]),
            common_dir: PathBuf::from(lines[2]),
        }))
    }

    /// True for the main checkout, false for a linked worktree.
    pub fn is_main_worktree(&self) -> bool {
        canonical(&self.git_dir) == canonical(&self.common_dir)
    }

    /// The commit `HEAD` names, or `None` on an unborn branch.
    pub fn head_commit(&self) -> Result<Option<String>> {
        let out = output(&self.root, ["rev-parse", "-q", "--verify", "HEAD^{commit}"])?;
        if out.status.success() {
            Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_owned()))
        } else {
            Ok(None)
        }
    }

    /// The object format's all-zero id (`0` × 40 for SHA-1, × 64 for SHA-256).
    pub fn zero_oid(&self) -> Result<String> {
        let format = run(&self.root, ["rev-parse", "--show-object-format"])?;
        Ok(match format.as_str() {
            "sha256" => "0".repeat(64),
            _ => "0".repeat(40),
        })
    }
}

/// The canonical form of a path, or the path itself when it cannot be resolved.
pub fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// One registered worktree, as `git worktree list --porcelain` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    /// The commit its `HEAD` names; `None` on an unborn branch.
    pub head: Option<String>,
    /// The full ref its `HEAD` names; `None` when detached.
    pub branch: Option<String>,
    /// Registered, but its folder is missing.
    pub prunable: bool,
    /// The first entry: the main checkout (or the bare repository).
    pub main: bool,
}

/// Every registered worktree of the clone `dir` belongs to, prunable ones included.
pub fn worktree_list(dir: &Path) -> Result<Vec<WorktreeEntry>> {
    let out = output(dir, ["worktree", "list", "--porcelain", "-z"])?;
    if !out.status.success() {
        return Err(Failure::refused(format!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let mut list: Vec<WorktreeEntry> = Vec::new();
    for field in out.stdout.split(|&b| b == 0) {
        let field = String::from_utf8_lossy(field);
        if let Some(p) = field.strip_prefix("worktree ") {
            let main = list.is_empty();
            list.push(WorktreeEntry {
                path: PathBuf::from(p),
                head: None,
                branch: None,
                prunable: false,
                main,
            });
            continue;
        }
        let Some(cur) = list.last_mut() else { continue };
        if let Some(h) = field.strip_prefix("HEAD ") {
            cur.head = Some(h.to_owned());
        } else if let Some(b) = field.strip_prefix("branch ") {
            cur.branch = Some(b.to_owned());
        } else if field == "prunable" || field.starts_with("prunable ") {
            cur.prunable = true;
        }
    }
    Ok(list)
}
