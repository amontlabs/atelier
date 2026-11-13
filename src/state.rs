//! `.git/atelier/` (DESIGN §2.2): the state every worktree of a clone shares, in two layers.
//!
//! **Frozen layer**, the same in every version, so any version may use it: `state.lock`, the lock
//! files under `locks/`, the run folders under `runs/`, the temp-folder registry under `tmp/`,
//! and the branch records under `branches/`. These paths never change, and their formats only
//! gain fields that older readers ignore (`docs/FROZEN-LAYER.md`).
//!
//! **Versioned layer**, under `versioned/`, with a format number in `versioned/format`:
//! approvals, the merge record, the promotion record. Only the version pinned on the integration
//! branch reads or writes it, and only in the main checkout on that branch.
//!
//! Every command takes `state.lock` shared before anything else and holds it until it exits. A
//! conversion of the versioned layer takes it exclusive, without waiting.

use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::exit::{Failure, IoContext, Result};
use crate::fsutil;
use crate::git::{self, Checkout};
use crate::names;
use crate::refs;
use crate::settings;

/// The versioned layer's format this build reads and writes.
pub const CURRENT_FORMAT: u32 = 1;

/// The folder name under the git common dir.
pub const STATE_DIR: &str = "atelier";

/// The shared state of one clone, with `state.lock` held shared for as long as it lives.
#[derive(Debug)]
pub struct State {
    root: PathBuf,
    lock: File,
}

impl State {
    /// Opens (creating if needed) `<git common dir>/atelier/` and takes `state.lock` shared,
    /// waiting while a conversion holds it exclusive.
    pub fn open(checkout: &Checkout) -> Result<State> {
        Self::open_at(checkout.common_dir.join(STATE_DIR))
    }

    pub fn open_at(root: PathBuf) -> Result<State> {
        for sub in ["", "locks", "runs", "tmp", "branches"] {
            create_dir(&root.join(sub))?;
        }
        let lock_path = root.join("state.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .ctx(lock_path.display())?;
        lock.lock_shared()
            .ctx(format!("{}: shared lock", lock_path.display()))?;
        Ok(State { root, lock })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Frozen layer: lock files.
    pub fn locks_dir(&self) -> PathBuf {
        self.root.join("locks")
    }

    /// Frozen layer: one folder per run.
    pub fn runs_dir(&self) -> PathBuf {
        self.root.join("runs")
    }

    /// Frozen layer: the temp-folder registry.
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Frozen layer: one record per branch, keyed by [`names::key`].
    pub fn branch_record_path(&self, branch: &str) -> PathBuf {
        self.root
            .join("branches")
            .join(format!("{}.toml", names::key(branch)))
    }

    /// Versioned layer folder. Use only through [`State::versioned`].
    fn versioned_dir(&self) -> PathBuf {
        self.root.join("versioned")
    }

    /// The versioned layer's format number; `None` when it was never created.
    pub fn format(&self) -> Result<Option<u32>> {
        let path = self.versioned_dir().join("format");
        match fs::read_to_string(&path) {
            Ok(text) => {
                text.trim().parse().map(Some).map_err(|_| {
                    Failure::refused(format!("{}: not a format number", path.display()))
                })
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Failure::refused(format!("{}: {e}", path.display()))),
        }
    }

    /// Opens the versioned layer for the version pinned on the integration branch: creates it
    /// when absent, converts an older format under `state.lock` taken exclusive without waiting,
    /// and refuses (4) a newer format. `converters[n - 1]` converts format `n` to `n + 1`;
    /// `current` must equal `converters.len() + 1`.
    pub fn versioned(&self, current: u32, converters: &[Converter]) -> Result<Versioned<'_>> {
        assert_eq!(
            current as usize,
            converters.len() + 1,
            "one converter per older format"
        );
        let dir = self.versioned_dir();
        match self.format()? {
            None => {
                create_dir(&dir)?;
                write_format_if_absent(&dir, current)?;
                // Another command may have created it first, with its own number.
                return self.versioned(current, converters);
            }
            Some(f) if f == current => {}
            Some(f) if f > current => {
                return Err(Failure::refused(format!(
                    "state format is newer: atelier follow (the state is format {f}, this atelier reads {current})"
                )));
            }
            Some(_) => self.convert(current, converters)?,
        }
        Ok(Versioned { dir, _state: self })
    }

    /// The versioned layer as it is, for `status`: read only, never created or converted.
    /// `Ok(None)` when it does not exist yet; refuses (4) a format other than `current`.
    pub fn versioned_view(&self, current: u32) -> Result<Option<Versioned<'_>>> {
        match self.format()? {
            None => Ok(None),
            Some(f) if f == current => Ok(Some(Versioned {
                dir: self.versioned_dir(),
                _state: self,
            })),
            Some(_) => Err(Failure::refused(
                self.format_line(current)
                    .err()
                    .unwrap_or_else(|| "state format differs".to_owned()),
            )),
        }
    }

    /// Reports whether the versioned layer matches, for `status`, which never converts.
    pub fn format_line(&self, current: u32) -> std::result::Result<(), String> {
        match self.format() {
            Ok(None) => Ok(()),
            Ok(Some(f)) if f == current => Ok(()),
            Ok(Some(f)) if f > current => Err(format!(
                "state format is newer: atelier follow (format {f}, this atelier reads {current})"
            )),
            Ok(Some(f)) => Err(format!(
                "state format {f} is older than {current}: the next command of the version pinned on main converts it"
            )),
            Err(e) => Err(e.message),
        }
    }

    fn convert(&self, current: u32, converters: &[Converter]) -> Result<()> {
        // flock converts this descriptor's shared lock to exclusive. If it fails, the shared
        // lock may have been dropped (BSD semantics), so it is taken again before returning.
        if self.lock.try_lock().is_err() {
            self.lock.lock_shared().ctx("state.lock")?;
            return Err(Failure::refused(
                "state upgrade waits for running atelier commands",
            ));
        }
        let result = (|| {
            while let Some(f) = self.format()?.filter(|f| *f < current) {
                if f == 0 {
                    return Err(Failure::refused(
                        "versioned/format: 0 is not a format number",
                    ));
                }
                self.convert_one(f, converters[(f - 1) as usize])?;
            }
            Ok(())
        })();
        self.lock.lock_shared().ctx("state.lock")?;
        result
    }

    /// Converts format `f` to `f + 1` into a new folder, then swaps the two folders in one
    /// rename. A crash at any point leaves either the old layer whole (the next conversion starts
    /// again from it) or the new one whole; the converter never sees its own half-done output.
    fn convert_one(&self, f: u32, converter: Converter) -> Result<()> {
        let dir = self.versioned_dir();
        let next = self.root.join("versioned.next");
        remove_tree(&next)?;
        create_dir(&next)?;
        converter(&dir, &next).map_err(|e| e.context(format!("converting state format {f}")))?;
        fsutil::write_atomic(&next.join("format"), format!("{}\n", f + 1).as_bytes())
            .ctx("versioned.next/format")?;
        sync_tree(&next).ctx("versioned.next")?;
        swap(&dir, &next).map_err(|e| {
            Failure::refused(format!(
                "cannot swap {} and {}: {e}",
                dir.display(),
                next.display()
            ))
        })?;
        File::open(&self.root)
            .and_then(|d| d.sync_all())
            .ctx(self.root.display())?;
        // `versioned.next` now holds the old format.
        remove_tree(&next)
    }
}

/// Converts the versioned layer of format `n`, in the first folder (read only), into format
/// `n + 1`, written into the second, empty folder. atelier writes the new `format` file and
/// flushes everything the converter wrote.
pub type Converter = fn(&Path, &Path) -> Result<()>;

/// Exchanges two folders atomically.
#[cfg(target_os = "macos")]
fn swap(a: &Path, b: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let a = std::ffi::CString::new(a.as_os_str().as_bytes())?;
    let b = std::ffi::CString::new(b.as_os_str().as_bytes())?;
    // SAFETY: two valid NUL-terminated paths.
    if unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_SWAP) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Exchanges two folders atomically.
#[cfg(target_os = "linux")]
fn swap(a: &Path, b: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let a = std::ffi::CString::new(a.as_os_str().as_bytes())?;
    let b = std::ffi::CString::new(b.as_os_str().as_bytes())?;
    // SAFETY: two valid NUL-terminated paths, relative to nothing (they are absolute or cwd).
    let r = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn remove_tree(p: &Path) -> Result<()> {
    match fs::remove_dir_all(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Failure::refused(format!("{}: {e}", p.display()))),
    }
}

/// Flushes every file and folder under `p` to disk, `p` included.
fn sync_tree(p: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(p)? {
        let path = entry?.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            sync_tree(&path)?;
        } else if meta.is_file() {
            File::open(&path)?.sync_all()?;
        }
    }
    File::open(p)?.sync_all()
}

/// Access to the versioned layer, for as long as the shared `state.lock` is held.
#[derive(Debug)]
pub struct Versioned<'a> {
    dir: PathBuf,
    _state: &'a State,
}

impl Versioned<'_> {
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

fn write_format_if_absent(dir: &Path, format: u32) -> Result<()> {
    let tmp = dir.join(format!(
        ".format.new-{}-{}",
        std::process::id(),
        fsutil::random_hex(4)?
    ));
    let written = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        std::io::Write::write_all(&mut f, format!("{format}\n").as_bytes())?;
        f.sync_all()
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(Failure::refused(format!("{}: {e}", tmp.display())));
    }
    // A hard link never replaces an existing file: the first creator wins.
    let r = fs::hard_link(&tmp, dir.join("format"));
    let _ = fs::remove_file(&tmp);
    match r {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
        Err(e) => return Err(Failure::refused(format!("versioned/format: {e}"))),
    }
    // The link and the temp file's removal are flushed with the folder; so is the folder's own
    // entry in the state folder, created just before.
    for d in [dir, dir.parent().unwrap_or(dir)] {
        File::open(d)
            .and_then(|f| f.sync_all())
            .map_err(|e| Failure::refused(format!("{}: {e}", d.display())))?;
    }
    Ok(())
}

fn create_dir(p: &Path) -> Result<()> {
    match fs::create_dir(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists && p.is_dir() => Ok(()),
        Err(e) => Err(Failure::refused(format!("{}: {e}", p.display()))),
    }
}

/// The settings text committed at a branch's tip, or `None` when the branch or the file does
/// not exist. The ref must be plain.
pub fn committed_settings(checkout: &Checkout, branch: &str) -> Result<Option<String>> {
    let refname = names::branch_ref(branch);
    let Some(commit) = refs::read(&checkout.root, &refname)? else {
        return Ok(None);
    };
    let out = git::output(
        &checkout.root,
        [
            "cat-file",
            "blob",
            &format!("{commit}:{}", settings::SETTINGS_PATH),
        ],
    )?;
    if !out.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
}

/// The atelier version pinned in the integration branch's own tree, or `None` when that branch
/// has no settings yet. The ref must be plain.
pub fn pin_on_integration_branch(checkout: &Checkout, branch: &str) -> Result<Option<String>> {
    let Some(text) = committed_settings(checkout, branch)? else {
        return Ok(None);
    };
    settings::read_pin(&text)
        .map(Some)
        .map_err(|f| f.context(format!("{branch}:{}", settings::SETTINGS_PATH)))
}

/// The integration branch as the main checkout sees it: the branch the main checkout's `HEAD`
/// names, and the integration branch the settings committed on that branch name. Neither comes
/// from the worktree a command runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MainBranch {
    /// The branch the main checkout is on; `None` when its `HEAD` is detached.
    pub head: Option<String>,
    /// `integration_branch` committed on `head`; `None` when `head` has no settings.
    pub integration: Option<String>,
}

impl MainBranch {
    pub fn read(checkout: &Checkout) -> Result<MainBranch> {
        // The main checkout's HEAD lives in the common git folder.
        let out = git::output(&checkout.common_dir, ["symbolic-ref", "-q", "HEAD"])?;
        let head = out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
            .and_then(|r| r.strip_prefix("refs/heads/").map(str::to_owned));
        let Some(head) = head else {
            return Ok(MainBranch {
                head: None,
                integration: None,
            });
        };
        let integration = match committed_settings(checkout, &head)? {
            Some(text) => Some(
                settings::read_integration_branch(&text)
                    .map_err(|f| f.context(format!("{head}:{}", settings::SETTINGS_PATH)))?,
            ),
            None => None,
        };
        Ok(MainBranch {
            head: Some(head),
            integration,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (tempfile::TempDir, State) {
        let dir = tempfile::tempdir().unwrap();
        let s = State::open_at(dir.path().join("atelier")).unwrap();
        (dir, s)
    }

    fn bump(from: &Path, to: &Path) -> Result<()> {
        assert!(from.join("format").is_file());
        assert_eq!(fs::read_dir(to).unwrap().count(), 0, "starts empty");
        fs::write(to.join("converted"), "yes").ctx("converted")
    }

    fn half_then_fail(_from: &Path, to: &Path) -> Result<()> {
        fs::write(to.join("half"), "written").ctx("half")?;
        Err(Failure::refused("crashed"))
    }

    #[test]
    fn creates_the_frozen_layout() {
        let (_d, s) = state();
        for sub in ["locks", "runs", "tmp", "branches"] {
            assert!(s.root().join(sub).is_dir());
        }
        assert!(s.root().join("state.lock").is_file());
        assert_eq!(s.format().unwrap(), None);
        assert_eq!(
            s.branch_record_path("a/b").file_name().unwrap(),
            "a%2Fb.toml"
        );
    }

    #[test]
    fn versioned_layer_is_created_with_the_current_format() {
        let (_d, s) = state();
        let v = s.versioned(CURRENT_FORMAT, &[]).unwrap();
        assert!(v.dir().is_dir());
        assert_eq!(s.format().unwrap(), Some(CURRENT_FORMAT));
    }

    #[test]
    fn newer_format_is_refused_and_untouched() {
        let (_d, s) = state();
        s.versioned(1, &[]).unwrap();
        fs::write(s.root().join("versioned/format"), "9\n").unwrap();
        let e = s.versioned(1, &[]).unwrap_err();
        assert_eq!(e.exit, crate::exit::Exit::Refused);
        assert!(
            e.message
                .starts_with("state format is newer: atelier follow"),
            "{}",
            e.message
        );
        assert_eq!(s.format().unwrap(), Some(9));
        assert!(s.format_line(1).is_err());
    }

    #[test]
    fn older_format_converts_when_no_other_command_runs() {
        let (_d, s) = state();
        s.versioned(1, &[]).unwrap();
        assert!(s.format_line(2).is_err(), "status reports, never converts");
        assert_eq!(s.format().unwrap(), Some(1));
        let v = s.versioned(2, &[bump]).unwrap();
        assert!(v.dir().join("converted").is_file());
        assert_eq!(s.format().unwrap(), Some(2));
        // The shared lock is held again after the conversion.
        let other = File::open(s.root().join("state.lock")).unwrap();
        assert!(other.try_lock().is_err());
        assert!(other.try_lock_shared().is_ok());
    }

    #[test]
    fn an_interrupted_conversion_reruns_from_the_old_layer() {
        let (_d, s) = state();
        let v = s.versioned(1, &[]).unwrap();
        fs::write(v.dir().join("approvals"), "old").unwrap();
        // The converter dies half-way: the old layer is untouched and still format 1.
        let e = s.versioned(2, &[half_then_fail]).unwrap_err();
        assert!(
            e.message.contains("converting state format 1"),
            "{}",
            e.message
        );
        assert_eq!(s.format().unwrap(), Some(1));
        assert_eq!(
            fs::read_to_string(s.root().join("versioned/approvals")).unwrap(),
            "old"
        );
        assert!(!s.root().join("versioned/half").exists());
        // A crash also leaves the half-written folder behind; the rerun starts from scratch.
        assert!(s.root().join("versioned.next/half").exists());
        let v = s.versioned(2, &[bump]).unwrap();
        assert!(v.dir().join("converted").is_file());
        assert!(!v.dir().join("half").exists());
        assert!(!s.root().join("versioned.next").exists());
        assert_eq!(s.format().unwrap(), Some(2));
    }

    #[test]
    fn the_format_file_is_written_whole() {
        let (_d, s) = state();
        s.versioned(1, &[]).unwrap();
        assert_eq!(
            fs::read_to_string(s.root().join("versioned/format")).unwrap(),
            "1\n"
        );
        let leftovers: Vec<_> = fs::read_dir(s.root().join("versioned"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers, vec![std::ffi::OsString::from("format")]);
    }

    #[test]
    fn conversion_refuses_while_another_command_holds_the_lock() {
        let (_d, s) = state();
        s.versioned(1, &[]).unwrap();
        // Another atelier command: its own open file description, shared.
        let other = State::open_at(s.root().to_path_buf()).unwrap();
        let e = s.versioned(2, &[bump]).unwrap_err();
        assert_eq!(
            e.message,
            "state upgrade waits for running atelier commands"
        );
        assert_eq!(s.format().unwrap(), Some(1));
        assert!(!s.root().join("versioned/converted").exists());
        drop(other);
        s.versioned(2, &[bump]).unwrap();
        assert_eq!(s.format().unwrap(), Some(2));
    }
}
