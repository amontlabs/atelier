//! Processes: process groups, stopping a group, and the environment-marker survivor scan
//! (r4 §3.1).
//!
//! Everything atelier starts leads its own process group and carries `ATELIER_RUN=<run id>`. A
//! group is checked with `kill(-pgid, 0)`; members are listed for messages. A program that starts
//! its own session leaves the group but keeps its environment, so at the end of a run atelier
//! reads the environment of this user's processes (on macOS through `KERN_PROCARGS2`, the data
//! `ps -E` prints, matched entry by entry; on Linux `/proc/<pid>/environ`) and looks for its own
//! marker. macOS hides the environment of Apple's own programs, and a program started under
//! `env -i` drops the marker: both are the accepted gaps of r4 §3.1.

use std::process::Child;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// The environment variable that marks every process of a run.
pub const RUN_MARKER: &str = "ATELIER_RUN";

/// How long a group gets between TERM and KILL, by default.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(5);

/// How long atelier waits for a group to disappear after KILL before reporting it.
const AFTER_KILL: Duration = Duration::from_secs(5);

const POLL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: i32,
    pub pgid: i32,
    pub command: String,
}

impl ProcInfo {
    /// `pid program` for messages.
    pub fn describe(&self) -> String {
        let cmd: String = self.command.chars().take(120).collect();
        format!("{} {}", self.pid, cmd)
    }
}

/// True while any process is in the group (zombies included), including processes of other
/// users that this user cannot signal.
pub fn group_alive(pgid: i32) -> bool {
    if pgid <= 1 {
        return false;
    }
    // SAFETY: kill with signal 0 only checks for existence and permission.
    let r = unsafe { libc::kill(-pgid, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The processes in a group, for messages.
pub fn group_members(pgid: i32) -> Vec<ProcInfo> {
    list()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.pgid == pgid)
        .collect()
}

fn signal_group(pgid: i32, sig: libc::c_int) {
    if pgid > 1 {
        // SAFETY: plain syscall; a negative pid addresses the process group.
        unsafe {
            libc::kill(-pgid, sig);
        }
    }
}

/// What stopping a group found and did.
#[derive(Debug, Clone, Default)]
pub struct Stopped {
    /// Members present when the stop began.
    pub members: Vec<ProcInfo>,
    /// KILL was needed.
    pub killed: bool,
    /// Members still present at the end (the group could not be emptied).
    pub remaining: Vec<ProcInfo>,
}

impl Stopped {
    pub fn was_empty(&self) -> bool {
        self.members.is_empty() && !self.killed
    }
}

/// Stops a group: TERM, wait up to `grace`, KILL, wait. `leader` is reaped meanwhile so that it
/// does not linger as a zombie of this process. Never touches any other process.
pub fn stop_group(pgid: i32, mut leader: Option<&mut Child>, grace: Duration) -> Stopped {
    let mut reap = |leader: &mut Option<&mut Child>| {
        if let Some(child) = leader.as_deref_mut() {
            let _ = child.try_wait();
        }
    };
    reap(&mut leader);
    let mut out = Stopped::default();
    if !group_alive(pgid) {
        return out;
    }
    out.members = group_members(pgid);
    signal_group(pgid, libc::SIGTERM);
    signal_group(pgid, libc::SIGCONT);
    if wait_empty(pgid, &mut leader, grace, &mut reap) {
        return out;
    }
    out.killed = true;
    signal_group(pgid, libc::SIGKILL);
    if !wait_empty(pgid, &mut leader, AFTER_KILL, &mut reap) {
        out.remaining = group_members(pgid);
    }
    out
}

fn wait_empty(
    pgid: i32,
    leader: &mut Option<&mut Child>,
    within: Duration,
    reap: &mut impl FnMut(&mut Option<&mut Child>),
) -> bool {
    let deadline = Instant::now() + within;
    loop {
        reap(leader);
        if !group_alive(pgid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(POLL);
    }
}

/// Processes, other than this one, whose environment holds `ATELIER_RUN=<id>` for one of `ids`.
/// Returns each with the run id it carries.
pub fn marker_survivors(ids: &[String]) -> std::io::Result<Vec<(ProcInfo, String)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let wanted: Vec<Vec<u8>> = ids
        .iter()
        .map(|id| format!("{RUN_MARKER}={id}").into_bytes())
        .collect();
    let me = std::process::id() as i32;
    let mut found = Vec::new();
    for p in list()? {
        if p.pid == me {
            continue;
        }
        let Some(env) = environ(p.pid) else { continue };
        if let Some(i) = wanted.iter().position(|w| env.iter().any(|e| e == w)) {
            found.push((p, ids[i].clone()));
        }
    }
    Ok(found)
}

/// Every process visible to this user, with its group and command line.
#[cfg(target_os = "macos")]
pub fn list() -> std::io::Result<Vec<ProcInfo>> {
    let out = std::process::Command::new("/bin/ps")
        .args(["-A", "-ww", "-o", "pid=,pgid=,command="])
        .stdin(std::process::Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other("ps failed"));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut procs = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(pgid)) = (it.next(), it.next()) else {
            continue;
        };
        let (Ok(pid), Ok(pgid)) = (pid.parse(), pgid.parse()) else {
            continue;
        };
        let command = it.collect::<Vec<_>>().join(" ");
        procs.push(ProcInfo { pid, pgid, command });
    }
    Ok(procs)
}

/// Every process visible to this user, with its group and command line.
#[cfg(target_os = "linux")]
pub fn list() -> std::io::Result<Vec<ProcInfo>> {
    let mut procs = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // "pid (comm) state ppid pgrp ...": comm may hold spaces and parentheses.
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        let Some(pgid) = fields.get(2).and_then(|s| s.parse().ok()) else {
            continue;
        };
        let comm = stat[..close]
            .split_once('(')
            .map(|x| x.1)
            .unwrap_or("")
            .to_owned();
        let command = std::fs::read(format!("/proc/{pid}/cmdline"))
            .ok()
            .map(|b| {
                b.split(|c| *c == 0)
                    .filter(|s| !s.is_empty())
                    .map(|s| String::from_utf8_lossy(s).into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or(comm);
        procs.push(ProcInfo { pid, pgid, command });
    }
    Ok(procs)
}

/// A process's environment entries, or `None` when the system does not show them.
#[cfg(target_os = "linux")]
pub fn environ(pid: i32) -> Option<Vec<Vec<u8>>> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    Some(
        raw.split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(<[u8]>::to_vec)
            .collect(),
    )
}

/// A process's environment entries, or `None` when the system does not show them (other users'
/// processes, Apple's own programs).
#[cfg(target_os = "macos")]
pub fn environ(pid: i32) -> Option<Vec<Vec<u8>>> {
    let buf = procargs2(pid)?;
    parse_procargs2(&buf)
}

#[cfg(target_os = "macos")]
fn procargs2(pid: i32) -> Option<Vec<u8>> {
    let mut argmax: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // SAFETY: reads one c_int into argmax; size describes it.
    let r = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&mut argmax as *mut libc::c_int).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 || argmax <= 0 {
        return None;
    }
    let mut buf = vec![0u8; argmax as usize];
    let mut size = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: the kernel writes at most `size` bytes into buf and updates size.
    let r = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return None;
    }
    buf.truncate(size);
    Some(buf)
}

/// Layout: `argc` (native i32), the executable path, NUL padding, `argc` arguments, then the
/// environment entries, each NUL-terminated, ending at an empty string.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_procargs2(buf: &[u8]) -> Option<Vec<Vec<u8>>> {
    if buf.len() < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?);
    if argc < 0 {
        return None;
    }
    let mut i = 4;
    // Executable path.
    while i < buf.len() && buf[i] != 0 {
        i += 1;
    }
    // Padding.
    while i < buf.len() && buf[i] == 0 {
        i += 1;
    }
    let next = |i: &mut usize| -> Option<Vec<u8>> {
        if *i >= buf.len() {
            return None;
        }
        let start = *i;
        while *i < buf.len() && buf[*i] != 0 {
            *i += 1;
        }
        let s = buf[start..*i].to_vec();
        *i += 1;
        Some(s)
    };
    for _ in 0..argc {
        next(&mut i)?;
    }
    let mut env = Vec::new();
    while let Some(entry) = next(&mut i) {
        if entry.is_empty() {
            break;
        }
        env.push(entry);
    }
    Some(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn procargs2_layout_is_parsed() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(
            b"/usr/bin/x\0\0\0\0x\0ATELIER_RUN=not-env\0A=1\0ATELIER_RUN=r1\0\0junk\0",
        );
        let env = parse_procargs2(&buf).unwrap();
        assert_eq!(env, vec![b"A=1".to_vec(), b"ATELIER_RUN=r1".to_vec()]);
    }

    #[test]
    fn this_process_is_listed_with_its_group() {
        let me = std::process::id() as i32;
        // SAFETY: getpgrp has no preconditions.
        let pgid = unsafe { libc::getpgrp() };
        let p = list().unwrap().into_iter().find(|p| p.pid == me).unwrap();
        assert_eq!(p.pgid, pgid);
        assert!(group_alive(pgid));
    }

    #[test]
    fn own_environment_is_readable() {
        let env = environ(std::process::id() as i32).expect("own environment");
        assert!(env.iter().any(|e| e.starts_with(b"PATH=")));
    }
}
