//! A file reached through directory descriptors, never through a path string that a link
//! could redirect: every folder on the way is opened with `O_NOFOLLOW` relative to the one
//! before it, and the file is read, checked and replaced (`openat`, `fstatat`, `renameat`)
//! relative to the descriptor of its own folder. Used by `follow` for the files it hands to
//! Mergiraf (3.3 review round 2, N4).
//!
//! Also the walks `clean` uses on a folder it deletes: [`survey`] looks at every entry without
//! following a link and reports what is on another device (a mount) and any nested `.git`;
//! [`remove_tree`] deletes a folder through descriptors, checking each folder's device before it
//! looks inside, so it never deletes anything on another file system.

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

/// What [`survey`] found under a folder, as paths relative to it.
#[derive(Debug, Default)]
pub struct Survey {
    /// Entries on another device than the folder: mount points, never entered.
    pub mounts: Vec<PathBuf>,
    /// Folders below the top that hold a `.git` (a file or a folder): nested repositories.
    pub repos: Vec<PathBuf>,
    /// Folders that could not be read, so what is inside them is unknown.
    pub unreadable: Vec<(PathBuf, io::Error)>,
}

/// Walks the folder `root` (itself not a link) without following links and without entering
/// another device.
pub fn survey(root: &Path) -> io::Result<Survey> {
    let fd = open_dir(root)?;
    let dev = fstat(&fd)?.st_dev;
    let mut found = Survey::default();
    survey_in(&fd, dev, &mut PathBuf::new(), &mut found);
    Ok(found)
}

fn survey_in(dir: &OwnedFd, dev: libc::dev_t, rel: &mut PathBuf, found: &mut Survey) {
    let names = match read_names(dir) {
        Ok(n) => n,
        Err(e) => {
            found.unreadable.push((rel.clone(), e));
            return;
        }
    };
    for name in names {
        rel.push(&name);
        match fstatat(dir, &name) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => found.unreadable.push((rel.clone(), e)),
            Ok(st) if st.st_dev != dev => found.mounts.push(rel.clone()),
            Ok(st) => {
                if name == ".git" {
                    if rel.components().count() > 1 {
                        found
                            .repos
                            .push(rel.parent().map(Path::to_path_buf).unwrap_or_default());
                    }
                } else if st.st_mode & libc::S_IFMT == libc::S_IFDIR {
                    match open_sub(dir, &name, dev) {
                        Ok(Some(child)) => survey_in(&child, dev, rel, found),
                        Ok(None) => found.mounts.push(rel.clone()),
                        Err(e) => found.unreadable.push((rel.clone(), e)),
                    }
                }
            }
        }
        rel.pop();
    }
}

/// Deletes the folder `name` in the folder `parent` (a canonical path) when it is a real folder
/// with the identity `expect`, and everything in it, through descriptors: no link is followed,
/// and a folder whose device is not `expect`'s is never entered or deleted (an error naming it).
/// Stops at the first error; what was deleted stays deleted.
pub fn remove_tree(parent: &Path, name: &OsStr, expect: Identity) -> io::Result<()> {
    let pfd = open_dir(parent)?;
    let fd = openat(&pfd, name, DIR_FLAGS | libc::O_NOFOLLOW, 0)?;
    if identity(&fstat(&fd)?) != expect {
        return Err(io::Error::other(format!(
            "{} is not the registered folder any more",
            parent.join(name).display()
        )));
    }
    let dev = expect.0 as libc::dev_t;
    let mut rel = PathBuf::from(name);
    empty(&fd, dev, &mut rel).map_err(|(at, e)| {
        io::Error::new(e.kind(), format!("{}: {e}", parent.join(at).display()))
    })?;
    drop(fd);
    unlink(&pfd, name, libc::AT_REMOVEDIR)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", parent.join(name).display())))
}

/// Deletes everything in `dir`; on an error, the path where it happened.
fn empty(
    dir: &OwnedFd,
    dev: libc::dev_t,
    rel: &mut PathBuf,
) -> std::result::Result<(), (PathBuf, io::Error)> {
    let names = read_names(dir).map_err(|e| (rel.clone(), e))?;
    for name in names {
        rel.push(&name);
        let st = match fstatat(dir, &name) {
            Ok(st) => st,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                rel.pop();
                continue;
            }
            Err(e) => return Err((rel.clone(), e)),
        };
        if st.st_dev != dev {
            return Err((rel.clone(), other_device()));
        }
        if st.st_mode & libc::S_IFMT == libc::S_IFDIR {
            match open_sub(dir, &name, dev) {
                Ok(Some(child)) => empty(&child, dev, rel)?,
                Ok(None) => return Err((rel.clone(), other_device())),
                Err(e) => return Err((rel.clone(), e)),
            }
            unlink(dir, &name, libc::AT_REMOVEDIR).map_err(|e| (rel.clone(), e))?;
        } else {
            unlink(dir, &name, 0).map_err(|e| (rel.clone(), e))?;
        }
        rel.pop();
    }
    Ok(())
}

fn other_device() -> io::Error {
    io::Error::other("on another device (a mount): not entered, not deleted")
}

const DIR_FLAGS: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;

fn open_dir(path: &Path) -> io::Result<OwnedFd> {
    let c = cstr(path.as_os_str())?;
    let raw = unsafe { libc::open(c.as_ptr(), DIR_FLAGS | libc::O_NOFOLLOW) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Opens a folder in `dir` without following a link; `None` when it is on another device.
fn open_sub(dir: &OwnedFd, name: &OsStr, dev: libc::dev_t) -> io::Result<Option<OwnedFd>> {
    let fd = openat(dir, name, DIR_FLAGS | libc::O_NOFOLLOW, 0)?;
    Ok((fstat(&fd)?.st_dev == dev).then_some(fd))
}

fn unlink(dir: &OwnedFd, name: &OsStr, flags: libc::c_int) -> io::Result<()> {
    let c = cstr(name)?;
    if unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The names in a folder, without `.` and `..`, read through a copy of its descriptor.
fn read_names(dir: &OwnedFd) -> io::Result<Vec<OsString>> {
    let copy = unsafe { libc::dup(dir.as_raw_fd()) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    let d = unsafe { libc::fdopendir(copy) };
    if d.is_null() {
        let e = io::Error::last_os_error();
        unsafe { libc::close(copy) };
        return Err(e);
    }
    // The copy shares the offset with `dir`: start from the beginning.
    unsafe { libc::rewinddir(d) };
    let mut names = Vec::new();
    let result = loop {
        // SAFETY: errno is thread-local; readdir signals an error only through it.
        unsafe { *errno() = 0 };
        let ent = unsafe { libc::readdir(d) };
        if ent.is_null() {
            let code = unsafe { *errno() };
            break if code == 0 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(code))
            };
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*ent).d_name.as_ptr()) };
        let bytes = name.to_bytes();
        if bytes != b"." && bytes != b".." {
            names.push(OsStr::from_bytes(bytes).to_owned());
        }
    };
    unsafe { libc::closedir(d) };
    result.map(|()| names)
}

#[cfg(target_os = "macos")]
fn errno() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

#[cfg(target_os = "linux")]
fn errno() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
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
