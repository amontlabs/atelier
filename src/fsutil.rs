//! Small file-system and clock helpers: atomic writes, random hex, UTC time stamps.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// A fresh temporary file next to `path`, holding `contents`, flushed to disk.
fn write_temp<'a>(
    path: &'a Path,
    contents: &[u8],
) -> std::io::Result<(std::path::PathBuf, &'a Path)> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other(format!("{} has no parent folder", path.display())))?;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other(format!("{} has no file name", path.display())))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}-{}",
        name.to_string_lossy(),
        std::process::id(),
        random_hex(4)?
    ));
    let written = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok((tmp, dir))
}

/// Writes `contents` to `path` atomically: a temporary file in the same folder, flushed to disk,
/// renamed over `path`, then the folder flushed. A reader sees the old file or the new one,
/// never a partial one.
pub fn write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let (tmp, dir) = write_temp(path, contents)?;
    let result = (|| {
        fs::rename(&tmp, path)?;
        File::open(dir)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Like [`write_atomic`], but fails with `AlreadyExists` instead of replacing a file at `path`.
pub fn write_atomic_new(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let (tmp, dir) = write_temp(path, contents)?;
    let result = fs::hard_link(&tmp, path);
    let _ = fs::remove_file(&tmp);
    result?;
    File::open(dir)?.sync_all()
}

/// Where a write to `path` lands: `path` itself, or the end of the symbolic link chain that
/// starts there (a link to a missing file resolves to that file's path).
pub fn resolve_link(path: &Path) -> std::io::Result<std::path::PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        match fs::symlink_metadata(&current) {
            Ok(m) if m.file_type().is_symlink() => {
                let target = fs::read_link(&current)?;
                current = if target.is_absolute() {
                    target
                } else {
                    current.parent().unwrap_or(Path::new("")).join(target)
                };
            }
            Ok(_) => return Ok(current),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(current),
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(format!(
        "{}: too many levels of symbolic links",
        path.display()
    )))
}

/// Writes `contents` where `path` resolves to (see [`resolve_link`]), atomically: a symbolic
/// link at `path` stays a link.
pub fn write_through(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    write_atomic(&resolve_link(path)?, contents)
}

/// Whether two paths are the same file: the same resolved path, or the same device and inode.
pub fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if let (Ok(ra), Ok(rb)) = (resolve_link(a), resolve_link(b))
        && ra == rb
    {
        return true;
    }
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// `n` random bytes from the system, as lowercase hex.
pub fn random_hex(n: usize) -> std::io::Result<String> {
    let mut buf = vec![0u8; n];
    File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Civil UTC date and time of a Unix time stamp (Howard Hinnant's days-to-civil algorithm).
pub fn utc_parts(secs: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

/// RFC 3339 UTC time stamp, second precision: `2026-11-13T10:12:03Z`.
pub fn rfc3339(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = utc_parts(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formatting_matches_known_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_794_571_923), "2026-11-13T12:12:03Z");
        assert_eq!(rfc3339(4_107_542_399), "2100-02-28T23:59:59Z");
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("result");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("result")]);
    }

    #[test]
    fn atomic_new_refuses_to_replace() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("result");
        write_atomic_new(&p, b"one").unwrap();
        let e = write_atomic_new(&p, b"two").unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&p).unwrap(), b"one");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn writes_go_through_links_and_keep_them() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        fs::create_dir(base.join("sub")).unwrap();
        fs::write(base.join("sub/real"), "old").unwrap();
        symlink("sub/real", base.join("link")).unwrap();
        symlink("link", base.join("link2")).unwrap();
        write_through(&base.join("link2"), b"new").unwrap();
        assert_eq!(fs::read(base.join("sub/real")).unwrap(), b"new");
        assert!(
            fs::symlink_metadata(base.join("link2"))
                .unwrap()
                .is_symlink()
        );
        assert!(
            fs::symlink_metadata(base.join("link"))
                .unwrap()
                .is_symlink()
        );
        assert!(same_file(&base.join("link2"), &base.join("sub/real")));
        assert!(!same_file(&base.join("link2"), &base.join("sub")));
        // A link to a missing file: the file is created at its target.
        symlink("sub/made", base.join("dangling")).unwrap();
        assert!(same_file(&base.join("dangling"), &base.join("sub/made")));
        write_through(&base.join("dangling"), b"x").unwrap();
        assert_eq!(fs::read(base.join("sub/made")).unwrap(), b"x");
        assert!(
            fs::symlink_metadata(base.join("dangling"))
                .unwrap()
                .is_symlink()
        );
    }

    #[test]
    fn random_hex_has_the_requested_length() {
        assert_eq!(random_hex(2).unwrap().len(), 4);
        assert_ne!(random_hex(16).unwrap(), random_hex(16).unwrap());
    }
}
