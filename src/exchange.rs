//! Exchanging two folders in one step: `renamex_np(RENAME_SWAP)` on macOS,
//! `renameat2(RENAME_EXCHANGE)` on Linux (glibc 2.28 or newer). Filesystems without it (NFS, SMB,
//! exFAT, some older Linux ones) fail with an error and change nothing.
//!
//! The file is also a program on its own, with no dependency: `machines init` compiles it with
//! `rustc` on the machine before the build, and runs it on the install folder, so a filesystem
//! that cannot exchange is refused before anything is built.

use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_uint};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn renamex_np(from: *const c_char, to: *const c_char, flags: c_uint) -> c_int;
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn renameat2(
        olddirfd: c_int,
        oldpath: *const c_char,
        newdirfd: c_int,
        newpath: *const c_char,
        flags: c_uint,
    ) -> c_int;
}

/// Exchanges two existing paths atomically.
pub fn exchange(a: &Path, b: &Path) -> std::io::Result<()> {
    let ca = CString::new(a.as_os_str().as_bytes())?;
    let cb = CString::new(b.as_os_str().as_bytes())?;
    #[cfg(target_os = "macos")]
    // SAFETY: two NUL-terminated paths; RENAME_SWAP is 0x2.
    let r = unsafe { renamex_np(ca.as_ptr(), cb.as_ptr(), 0x2) };
    #[cfg(target_os = "linux")]
    // SAFETY: two NUL-terminated paths relative to the current folder (AT_FDCWD is -100);
    // RENAME_EXCHANGE is 0x2.
    let r = unsafe { renameat2(-100, ca.as_ptr(), -100, cb.as_ptr(), 0x2) };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Exchanges two new empty folders inside `dir`, checks that they changed places and removes
/// them. `Err` says why `dir` cannot exchange folders.
pub fn probe(dir: &Path) -> std::result::Result<(), String> {
    let tag = format!(".exchange-probe-{}", std::process::id());
    let a = dir.join(format!("{tag}-a"));
    let b = dir.join(format!("{tag}-b"));
    let cleanup = || {
        let _ = std::fs::remove_file(a.join("mark"));
        let _ = std::fs::remove_file(b.join("mark"));
        let _ = std::fs::remove_dir(&a);
        let _ = std::fs::remove_dir(&b);
    };
    let result = (|| {
        std::fs::create_dir(&a).map_err(|e| format!("{}: {e}", a.display()))?;
        std::fs::create_dir(&b).map_err(|e| format!("{}: {e}", b.display()))?;
        std::fs::write(a.join("mark"), b"").map_err(|e| format!("{}: {e}", a.display()))?;
        exchange(&a, &b)
            .map_err(|e| format!("exchanging two folders in {}: {e}", dir.display()))?;
        if b.join("mark").is_file() && !a.join("mark").exists() {
            Ok(())
        } else {
            Err(format!(
                "exchanging two folders in {} reported success but did not exchange them",
                dir.display()
            ))
        }
    })();
    cleanup();
    result
}

/// `exchange-probe <folder>`: exit 0 when `<folder>` can exchange folders, else 1 and why.
#[allow(dead_code)]
fn main() {
    let Some(dir) = std::env::args_os().nth(1) else {
        eprintln!("usage: exchange-probe <folder>");
        std::process::exit(2);
    };
    if let Err(e) = probe(Path::new(&dir)) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
