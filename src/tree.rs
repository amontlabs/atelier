//! The tree id (r4 §3.1, B5): the git tree object of everything in the worktree that git does not
//! ignore, tracked and untracked, hashed by content through a temporary index. The real index is
//! never touched. A tree holding a submodule or a nested repository (mode 160000) is refused (4):
//! its files are not part of the tree object, so a change inside it would go unseen.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::exit::{Failure, Result};
use crate::fsutil;
use crate::git::{self, Checkout};

/// A temporary index file, removed (with its lock file) when dropped.
struct TempIndex(PathBuf);

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = fs::remove_file(lock);
    }
}

const INDEX_PREFIX: &str = "atelier-tree-index-";

/// Removes temporary indexes left by an atelier that was killed: those whose process (named in
/// the file name) no longer exists. A reused pid only keeps a leftover a little longer.
fn remove_stale_indexes(git_dir: &std::path::Path) {
    let Ok(entries) = fs::read_dir(git_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(INDEX_PREFIX)) else {
            continue;
        };
        let Some(pid) = rest.split('-').next().and_then(|p| p.parse::<i32>().ok()) else {
            continue;
        };
        // SAFETY: existence check only.
        let gone = pid > 0
            && unsafe { libc::kill(pid, 0) } != 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Computes the tree id of the worktree. Refuses (4) a tree with an entry of mode 160000 and a
/// tree git cannot record (a nested repository without a commit, for one).
pub fn tree_id(checkout: &Checkout) -> Result<String> {
    let real_index = PathBuf::from(git::run(
        &checkout.root,
        ["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?);
    remove_stale_indexes(&checkout.git_dir);
    let tmp = TempIndex(checkout.git_dir.join(format!(
        "{INDEX_PREFIX}{}-{}",
        std::process::id(),
        fsutil::random_hex(6)?
    )));
    if real_index.is_file() {
        fs::copy(&real_index, &tmp.0)
            .map_err(|e| Failure::refused(format!("copying the index: {e}")))?;
    }

    clear_index_bits(checkout, &tmp.0)?;

    let add = git::command(&checkout.root)
        .env("GIT_INDEX_FILE", &tmp.0)
        .args(["-c", "advice.addEmbeddedRepo=false", "add", "-A"])
        .output()
        .map_err(|e| Failure::refused(format!("cannot run git: {e}")))?;
    if !add.status.success() {
        return Err(Failure::refused(format!(
            "git cannot record this tree (a nested repository or submodule?): {}",
            String::from_utf8_lossy(&add.stderr).trim()
        )));
    }
    let write = git::command(&checkout.root)
        .env("GIT_INDEX_FILE", &tmp.0)
        .args(["write-tree"])
        .output()
        .map_err(|e| Failure::refused(format!("cannot run git: {e}")))?;
    if !write.status.success() {
        return Err(Failure::refused(format!(
            "git cannot record this tree: {}",
            String::from_utf8_lossy(&write.stderr).trim()
        )));
    }
    let tree = String::from_utf8_lossy(&write.stdout).trim().to_owned();
    refuse_gitlinks(checkout, &tree)?;
    // A committed submodule whose folder is missing drops out of the worktree's tree, but the
    // project still has one.
    if let Some(head) = head_tree(checkout)? {
        refuse_gitlinks(checkout, &head)?;
    }
    Ok(tree)
}

/// Clears the assume-unchanged and skip-worktree bits of every entry of the temporary index, so
/// `git add -A` reads each file: with the bits set it would keep the committed content of a file
/// that changed on disk.
fn clear_index_bits(checkout: &Checkout, index: &Path) -> Result<()> {
    let listed = git::command(&checkout.root)
        .env("GIT_INDEX_FILE", index)
        .args(["ls-files", "-v", "-z"])
        .output()
        .map_err(|e| Failure::refused(format!("cannot run git: {e}")))?;
    if !listed.status.success() {
        return Err(Failure::refused(format!(
            "git ls-files failed: {}",
            String::from_utf8_lossy(&listed.stderr).trim()
        )));
    }
    // `<tag> <path>\0`: a lowercase tag marks assume-unchanged, `S` skip-worktree.
    let mut paths = Vec::new();
    for entry in listed.stdout.split(|b| *b == 0).filter(|e| e.len() > 2) {
        if entry[0].is_ascii_lowercase() || entry[0] == b'S' {
            paths.extend_from_slice(&entry[2..]);
            paths.push(0);
        }
    }
    if paths.is_empty() {
        return Ok(());
    }
    // One flag per call: `update-index` applies only the first of them to a path.
    for flag in ["--no-assume-unchanged", "--no-skip-worktree"] {
        let mut child = git::command(&checkout.root)
            .env("GIT_INDEX_FILE", index)
            .args(["update-index", "-z", flag, "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Failure::refused(format!("cannot run git: {e}")))?;
        let mut stdin = child.stdin.take().expect("piped stdin");
        // Written from a thread so that git's error output cannot fill a pipe nobody reads.
        let list = paths.clone();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&list);
        });
        let out = child
            .wait_with_output()
            .map_err(|e| Failure::refused(format!("cannot run git: {e}")))?;
        let _ = writer.join();
        if !out.status.success() {
            return Err(Failure::refused(format!(
                "git update-index {flag} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
    }
    Ok(())
}

/// Refuses (4) a tree that contains any entry of mode 160000, naming each.
pub fn refuse_gitlinks(checkout: &Checkout, tree: &str) -> Result<()> {
    let out = git::output(&checkout.root, ["ls-tree", "-r", "-z", "--full-tree", tree])?;
    if !out.status.success() {
        return Err(Failure::refused(format!(
            "git ls-tree {tree} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let mut links = Vec::new();
    for entry in out.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        if entry.starts_with(b"160000 ") {
            let path = entry
                .iter()
                .position(|b| *b == b'\t')
                .map(|i| String::from_utf8_lossy(&entry[i + 1..]).into_owned())
                .unwrap_or_default();
            links.push(path);
        }
    }
    if links.is_empty() {
        Ok(())
    } else {
        Err(Failure::refused(format!(
            "submodules and nested repositories are not supported; this tree has: {}",
            links.join(", ")
        )))
    }
}

/// The tree of the commit `HEAD` names, `None` on an unborn branch.
pub fn head_tree(checkout: &Checkout) -> Result<Option<String>> {
    let out = git::output(
        &checkout.root,
        ["rev-parse", "-q", "--verify", "HEAD^{tree}"],
    )?;
    Ok(out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()))
}
