//! Running git. Every call names its folder and drops the environment variables that would make
//! git act on another repository, index or ref namespace than the checkout atelier runs in.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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

/// A git command in a given folder.
pub fn command(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).stdin(Stdio::null());
    for var in REDIRECTING_VARS {
        cmd.env_remove(var);
    }
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
