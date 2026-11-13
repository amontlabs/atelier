//! A file reached through directory descriptors, never through a path string that a link
//! could redirect: every folder on the way is opened with `O_NOFOLLOW` relative to the one
//! before it, and the file is read, checked and replaced (`openat`, `fstatat`, `renameat`)
//! relative to the descriptor of its own folder. Used by `follow` for the files it hands to
//! Mergiraf (3.3 review round 2, N4).

use std::ffi::{CString, OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// The identity of a file-system object: device and inode.
pub type Identity = (u64, u64);

/// A regular file as it was read: its identity, its permission bits and its contents.
pub struct Snapshot {
    pub identity: Identity,
    pub mode: u32,
    pub contents: Vec<u8>,
}

/// The folder holding a file under a root, opened folder by folder without following links.
pub struct Parent {
    root: PathBuf,
    folders: Vec<OsString>,
    /// The identity of each folder on the way, the root's first.
    chain: Vec<Identity>,
    fd: OwnedFd,
    name: OsString,
}

impl Parent {
    /// Opens the folder holding `rel` under `root`. `None` when `rel` is not a plain relative
    /// path, or a folder on the way is not a real folder (a link, a file, missing).
    pub fn open(root: &Path, rel: &str) -> Option<Parent> {
        let comps: Vec<_> = Path::new(rel).components().collect();
        if comps.is_empty() || !comps.iter().all(|c| matches!(c, Component::Normal(_))) {
            return None;
        }
        let (name, folders) = comps.split_last()?;
        let folders: Vec<OsString> = folders.iter().map(|c| c.as_os_str().to_owned()).collect();
        let (fd, chain) = walk(root, &folders).ok()?;
        Some(Parent {
            root: root.to_path_buf(),
            folders,
            chain,
            fd,
            name: name.as_os_str().to_owned(),
        })
    }

    /// True when the same path from the root still reaches the same folders, none of them
    /// through a link.
    pub fn still_there(&self) -> bool {
        walk(&self.root, &self.folders).is_ok_and(|(_, chain)| chain == self.chain)
    }

    /// The identity of the file's name in this folder when it is a regular file (not followed
    /// if it is a link).
    pub fn regular(&self) -> Option<Identity> {
        let st = fstatat(&self.fd, &self.name).ok()?;
        is_regular(&st).then(|| identity(&st))
    }

    /// Reads the file when it is a regular file, opened without following a link and without
    /// blocking on a special file.
    pub fn read(&self) -> Option<Snapshot> {
        let fd = openat(
            &self.fd,
            &self.name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            0,
        )
        .ok()?;
        let st = fstat(&fd).ok()?;
        if !is_regular(&st) {
            return None;
        }
        let mut contents = Vec::new();
        File::from(fd).read_to_end(&mut contents).ok()?;
        Some(Snapshot {
            identity: identity(&st),
            mode: u32::from(st.st_mode) & 0o7777,
            contents,
        })
    }

    /// Replaces the file by a new regular file with these contents and permission bits: a
    /// temporary file created in this folder (never through a link), flushed, then renamed over
    /// the name. Nothing is written to whatever the name pointed to.
    pub fn replace(&self, contents: &[u8], mode: u32) -> io::Result<()> {
        let mut tmp = OsString::from(".atelier-restore-");
        tmp.push(crate::fsutil::random_hex(8)?);
        let fd = openat(
            &self.fd,
            &tmp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )?;
        let written = (|| {
            // Set after creation: the umask applies to `openat`'s mode, not to `fchmod`.
            if unsafe { libc::fchmod(fd.as_raw_fd(), mode as libc::mode_t) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut f = File::from(fd);
            f.write_all(contents)?;
            f.sync_all()?;
            let (from, to) = (cstr(&tmp)?, cstr(&self.name)?);
            let fd = self.fd.as_raw_fd();
            if unsafe { libc::renameat(fd, from.as_ptr(), fd, to.as_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })();
        if written.is_err()
            && let Ok(t) = cstr(&tmp)
        {
            unsafe { libc::unlinkat(self.fd.as_raw_fd(), t.as_ptr(), 0) };
        }
        written
    }
}

/// Opens `root`, then each folder under it with `O_NOFOLLOW`, each relative to the one before;
/// returns the last folder's descriptor and every folder's identity, the root's first.
fn walk(root: &Path, folders: &[OsString]) -> io::Result<(OwnedFd, Vec<Identity>)> {
    let dir = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    let c = cstr(root.as_os_str())?;
    let raw = unsafe { libc::open(c.as_ptr(), dir) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut chain = vec![identity(&fstat(&fd)?)];
    for f in folders {
        fd = openat(&fd, f, dir | libc::O_NOFOLLOW, 0)?;
        chain.push(identity(&fstat(&fd)?));
    }
    Ok((fd, chain))
}

fn openat(
    dir: &OwnedFd,
    name: &OsStr,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> io::Result<OwnedFd> {
    let c = cstr(name)?;
    let raw = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags, mode) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn fstat(fd: &OwnedFd) -> io::Result<libc::stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

fn fstatat(dir: &OwnedFd, name: &OsStr) -> io::Result<libc::stat> {
    let c = cstr(name)?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            c.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

fn is_regular(st: &libc::stat) -> bool {
    st.st_mode & libc::S_IFMT == libc::S_IFREG
}

#[allow(clippy::unnecessary_cast)]
fn identity(st: &libc::stat) -> Identity {
    (st.st_dev as u64, st.st_ino as u64)
}

fn cstr(s: &OsStr) -> io::Result<CString> {
    CString::new(s.as_bytes()).map_err(|_| io::Error::other("a path with a NUL byte"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn a_linked_folder_on_the_way_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("f.rs"), "outside\n").unwrap();
        symlink(outside.path(), t.path().join("d")).unwrap();
        assert!(Parent::open(t.path(), "d/f.rs").is_none());
        assert!(Parent::open(t.path(), "../f.rs").is_none());
    }

    #[test]
    fn a_link_is_never_read_and_is_replaced_not_written_through() {
        let t = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("t.rs");
        fs::write(&target, "outside\n").unwrap();
        fs::create_dir(t.path().join("d")).unwrap();
        symlink(&target, t.path().join("d/f.rs")).unwrap();
        let p = Parent::open(t.path(), "d/f.rs").unwrap();
        assert!(p.read().is_none());
        assert!(p.regular().is_none());
        p.replace(b"inside\n", 0o644).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "outside\n");
        let meta = fs::symlink_metadata(t.path().join("d/f.rs")).unwrap();
        assert!(meta.is_file());
    }

    #[test]
    fn replace_keeps_the_given_permission_bits_and_a_swapped_folder_is_seen() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("d")).unwrap();
        let f = t.path().join("d/f.rs");
        fs::write(&f, "one\n").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o640)).unwrap();
        let p = Parent::open(t.path(), "d/f.rs").unwrap();
        let snap = p.read().unwrap();
        assert_eq!(snap.mode, 0o640);
        assert_eq!(p.regular(), Some(snap.identity));
        p.replace(b"two\n", snap.mode).unwrap();
        assert_eq!(fs::read_to_string(&f).unwrap(), "two\n");
        assert_eq!(
            fs::metadata(&f).unwrap().permissions().mode() & 0o7777,
            0o640
        );
        assert!(p.still_there());

        let outside = tempfile::tempdir().unwrap();
        fs::rename(t.path().join("d"), t.path().join("moved")).unwrap();
        symlink(outside.path(), t.path().join("d")).unwrap();
        assert!(!p.still_there());
    }
}
