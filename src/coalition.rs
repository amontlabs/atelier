//! launchd resource coalitions (DESIGN §8.4, experiment P22): on macOS every step runs as a
//! launchd job ([`crate::launchd`]), and every process the step starts keeps the job's resource
//! coalition, even after `setsid`, a double fork, `env -i`, or as a system program. A survivor
//! is a live process whose coalition is one of the run's steps'.
//!
//! The coalition of a process is read with `proc_pidinfo(PROC_PIDCOALITIONINFO)`, which is
//! unprivileged and also reads other users' processes. The call is undocumented, so before a
//! macOS version is trusted the preflight runs the `setsid` case once ([`ensure_probed`]) and
//! refuses (4) when the coalition does not catch it.
//!
//! A coalition is written `<id>@<boot session uuid>`: coalition ids only grow within one boot,
//! and the boot session tells a record of an earlier boot apart.

use std::fmt;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use crate::procs::{ProcInfo, Stopped};

/// A step's coalition: the resource coalition id and the boot session it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coalition {
    pub id: u64,
    pub boot: String,
}

impl fmt::Display for Coalition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.id, self.boot)
    }
}

impl Coalition {
    /// Reads `<id>@<boot>`; `None` for anything else.
    pub fn parse(s: &str) -> Option<Coalition> {
        let (id, boot) = s.split_once('@')?;
        let id: u64 = id.parse().ok().filter(|i| *i > 0)?;
        if boot.is_empty() || boot.contains(char::is_whitespace) || boot.contains('@') {
            return None;
        }
        Some(Coalition {
            id,
            boot: boot.to_owned(),
        })
    }

    /// True when the coalition belongs to this boot: only then can it have members.
    pub fn this_boot(&self) -> bool {
        boot_session().is_some_and(|b| b == self.boot)
    }
}

/// The boot session's id (`kern.bootsessionuuid`), `None` when it cannot be read.
#[cfg(target_os = "macos")]
pub fn boot_session() -> Option<String> {
    let name = c"kern.bootsessionuuid";
    let mut buf = [0u8; 128];
    let mut size = buf.len();
    // SAFETY: the kernel writes at most `size` bytes into buf and updates size.
    let r = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 || size == 0 {
        return None;
    }
    let s = String::from_utf8_lossy(&buf[..size])
        .trim_end_matches('\0')
        .trim()
        .to_owned();
    (!s.is_empty() && !s.contains(char::is_whitespace) && !s.contains('@')).then_some(s)
}

/// The boot session's id; Linux has no coalitions.
#[cfg(not(target_os = "macos"))]
pub fn boot_session() -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
const PROC_PIDCOALITIONINFO: libc::c_int = 20;

/// `struct proc_pidcoalitioninfo`: the coalition ids by type (resource, jetsam), then reserved.
#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Default)]
struct PidCoalitionInfo {
    ids: [u64; 2],
    reserved: [u64; 3],
}

/// The resource coalition of a process; `None` when it is gone or cannot be read.
#[cfg(target_os = "macos")]
pub fn of(pid: i32) -> Option<u64> {
    if pid <= 0 {
        return None;
    }
    let mut info = PidCoalitionInfo::default();
    let size = std::mem::size_of::<PidCoalitionInfo>() as libc::c_int;
    // SAFETY: proc_pidinfo writes at most `size` bytes into info.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            PROC_PIDCOALITIONINFO,
            0,
            (&mut info as *mut PidCoalitionInfo).cast(),
            size,
        )
    };
    (n == size && info.ids[0] != 0).then_some(info.ids[0])
}

/// Every pid on the machine.
#[cfg(target_os = "macos")]
fn all_pids() -> Vec<i32> {
    for _ in 0..4 {
        // SAFETY: a size query.
        let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if n <= 0 {
            return Vec::new();
        }
        let mut buf = vec![0i32; n as usize + 256];
        let bytes = (buf.len() * std::mem::size_of::<i32>()) as libc::c_int;
        // SAFETY: the kernel writes at most `bytes` bytes into buf.
        let got = unsafe { libc::proc_listallpids(buf.as_mut_ptr().cast(), bytes) };
        if got < 0 {
            continue;
        }
        let got = got as usize;
        if got >= buf.len() {
            continue;
        }
        buf.truncate(got);
        buf.retain(|p| *p > 0);
        return buf;
    }
    Vec::new()
}

/// True when the process is a zombie; false when it cannot be read (another user's process
/// counts as live).
#[cfg(target_os = "macos")]
fn zombie(pid: i32) -> bool {
    // SAFETY: proc_bsdinfo is plain data; proc_pidinfo fills it.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: proc_pidinfo writes at most `size` bytes into info.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    n == size && info.pbi_status == 5 // SZOMB
}

/// Live processes (zombies left out) whose coalition is one of `ids`, other than this process,
/// each with its coalition. Coalitions of another boot have no members.
#[cfg(target_os = "macos")]
pub fn members(ids: &[Coalition]) -> Vec<(i32, u64)> {
    let ours: Vec<u64> = ids.iter().filter(|c| c.this_boot()).map(|c| c.id).collect();
    if ours.is_empty() {
        return Vec::new();
    }
    let me = std::process::id() as i32;
    all_pids()
        .into_iter()
        .filter(|p| *p != me)
        .filter_map(|p| of(p).filter(|c| ours.contains(c)).map(|c| (p, c)))
        .filter(|(p, _)| !zombie(*p))
        .collect()
}

/// The members for messages: `pid program arguments`, or `pid ?` when the process cannot be
/// read (another user's).
#[cfg(target_os = "macos")]
pub fn describe(pids: &[i32]) -> Vec<ProcInfo> {
    if pids.is_empty() {
        return Vec::new();
    }
    let listed = crate::procs::list().unwrap_or_default();
    pids.iter()
        .map(|pid| {
            listed
                .iter()
                .find(|p| p.pid == *pid)
                .cloned()
                .unwrap_or(ProcInfo {
                    pid: *pid,
                    pgid: -1,
                    command: "?".into(),
                })
        })
        .collect()
}

/// Sends `sig` to every member of `ids`, re-reading each one's coalition just before: a pid
/// that left (ended and was reused) is never signalled, nor is this process. Returns the pids
/// signalled.
#[cfg(target_os = "macos")]
fn signal_members(ids: &[Coalition], sig: libc::c_int) -> Vec<i32> {
    let mut sent = Vec::new();
    for (pid, c) in members(ids) {
        if of(pid) == Some(c) {
            // SAFETY: plain syscall to one pid whose coalition was just checked.
            if unsafe { libc::kill(pid, sig) } == 0 {
                sent.push(pid);
            }
        }
    }
    sent
}

/// Stops every member of `ids`: TERM (and CONT), wait up to `grace`, KILL, wait; up to five
/// rounds while new members appear. Members still there at the end are `remaining`.
#[cfg(target_os = "macos")]
pub fn stop(ids: &[Coalition], grace: Duration) -> Stopped {
    let mut out = Stopped::default();
    let first: Vec<i32> = members(ids).into_iter().map(|(p, _)| p).collect();
    if first.is_empty() {
        return out;
    }
    out.members = describe(&first);
    for _ in 0..5 {
        if members(ids).is_empty() {
            break;
        }
        signal_members(ids, libc::SIGTERM);
        signal_members(ids, libc::SIGCONT);
        if wait_empty(ids, grace) {
            break;
        }
        out.killed = true;
        signal_members(ids, libc::SIGKILL);
        if wait_empty(ids, Duration::from_secs(5)) {
            break;
        }
    }
    let left: Vec<i32> = members(ids).into_iter().map(|(p, _)| p).collect();
    out.remaining = describe(&left);
    out
}

#[cfg(target_os = "macos")]
fn wait_empty(ids: &[Coalition], within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if members(ids).is_empty() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The coalitions file of a run folder: one line per step job of the run and of every run
/// nested in it, `<coalition> <launchd label> [<later fields>...]` (`docs/FROZEN-LAYER.md`).
pub const RUN_FILE: &str = "coalitions";

/// One line of a run's coalitions file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobLine {
    pub coalition: Coalition,
    pub label: String,
}

/// Reads a run's coalitions file: `None` when there is none (a run of a version without
/// coalitions, or a Linux run). Lines this version cannot read are skipped.
pub fn read_run_file(run_dir: &std::path::Path) -> std::io::Result<Option<Vec<JobLine>>> {
    let text = match std::fs::read_to_string(run_dir.join(RUN_FILE)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    Ok(Some(
        text.lines()
            .filter_map(|l| {
                let mut it = l.split(' ');
                let coalition = Coalition::parse(it.next()?)?;
                let label = it.next()?.to_owned();
                Some(JobLine { coalition, label })
            })
            .collect(),
    ))
}

/// Appends a line to a run's coalitions file, in one write.
pub fn append_run_file(run_dir: &std::path::Path, line: &JobLine) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(run_dir.join(RUN_FILE))?;
    f.write_all(format!("{} {}\n", line.coalition, line.label).as_bytes())
}

// ------------------------------------------------------------------------------- the probe

/// The probe record: `<state>/probes/coalition-<uid>.toml`, the macOS version the `setsid` case
/// passed on for this user.
#[cfg(target_os = "macos")]
fn probe_record(state: &crate::state::State) -> std::path::PathBuf {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    state
        .root()
        .join("probes")
        .join(format!("coalition-{uid}.toml"))
}

#[cfg(target_os = "macos")]
fn sysctl_string(name: &std::ffi::CStr) -> Option<String> {
    let mut buf = [0u8; 256];
    let mut size = buf.len();
    // SAFETY: the kernel writes at most `size` bytes into buf and updates size.
    let r = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (r == 0).then(|| {
        String::from_utf8_lossy(&buf[..size])
            .trim_end_matches('\0')
            .trim()
            .to_owned()
    })
}

/// This macOS's version and build, `26.5 (25F71)`.
#[cfg(target_os = "macos")]
pub fn macos_version() -> String {
    format!(
        "{} ({})",
        sysctl_string(c"kern.osproductversion").unwrap_or_else(|| "?".into()),
        sysctl_string(c"kern.osversion").unwrap_or_else(|| "?".into())
    )
}

#[cfg(target_os = "macos")]
#[derive(serde::Serialize, serde::Deserialize)]
struct ProbeRecord {
    macos: String,
    passed: String,
    atelier_version: String,
}

/// Runs the `setsid` case once per macOS version for this user and refuses (4) when the
/// coalition did not catch it, or launchd jobs cannot be started. A pass is recorded in the
/// frozen layer; a failure is not, so the next command probes again.
#[cfg(target_os = "macos")]
pub fn ensure_probed(state: &crate::state::State) -> crate::exit::Result<()> {
    use crate::exit::Failure;
    let path = probe_record(state);
    let version = macos_version();
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Ok(rec) = toml::from_str::<ProbeRecord>(&text)
        && rec.macos == version
    {
        return Ok(());
    }
    let dir = path.with_extension("job");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| Failure::refused(format!("{}: {e}", dir.display())))?;
    let found = crate::launchd::probe(&dir);
    let _ = std::fs::remove_dir_all(&dir);
    match found {
        Ok(()) => {
            let rec = ProbeRecord {
                macos: version,
                passed: crate::fsutil::rfc3339(crate::fsutil::now_secs()),
                atelier_version: crate::dispatch::VERSION.to_owned(),
            };
            let text = toml::to_string(&rec).map_err(|e| Failure::refused(e.to_string()))?;
            crate::fsutil::write_atomic(&path, text.as_bytes())
                .map_err(|e| Failure::refused(format!("{}: {e}", path.display())))?;
            Ok(())
        }
        Err(why) => Err(Failure::refused(format!(
            "atelier runs every step as a launchd job and finds what it leaves behind by its coalition; \
             on this Mac (macOS {version}) that does not work: {why}. atelier does not fall back to a weaker \
             scan, so no run can start here"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalitions_read_back_what_they_write() {
        let c = Coalition {
            id: 345021,
            boot: "8B0C-11".into(),
        };
        assert_eq!(Coalition::parse(&c.to_string()), Some(c));
        assert_eq!(Coalition::parse("0@x"), None);
        assert_eq!(Coalition::parse("12"), None);
        assert_eq!(Coalition::parse("12@"), None);
        assert_eq!(Coalition::parse("x@y"), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn this_process_has_a_coalition_and_a_boot_session() {
        assert!(of(std::process::id() as i32).is_some());
        assert!(boot_session().is_some());
        let other_boot = Coalition {
            id: of(std::process::id() as i32).unwrap(),
            boot: "another-boot".into(),
        };
        assert!(!other_boot.this_boot());
        assert!(members(&[other_boot]).is_empty());
    }
}
