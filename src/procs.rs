//! Processes: process groups, stopping a group, process start times, and the environment-marker
//! survivor scan (r4 §3.1).
//!
//! Everything atelier starts leads its own process group and carries `ATELIER_RUN=<run id>` and
//! `ATELIER_RUNS=<outer ids>:<run id>`. The second lists every run it is nested in, so an outer
//! run also finds what an inner atelier left behind. A program that starts its own session leaves
//! the group but keeps its environment, so at the end of a run atelier reads the environment of
//! this user's processes (on macOS through `KERN_PROCARGS2`, the data `ps -E` prints, matched
//! entry by entry; on Linux `/proc/<pid>/environ`) and looks for its own marker. macOS hides the
//! environment of Apple's own programs, and a program started under `env -i` drops the marker:
//! both are the accepted gaps of r4 §3.1.
//!
//! A recorded group is identified by its pgid and the start time of its leader ([`start_time`]),
//! so a pgid reused by an unrelated process is told apart from the group atelier started.

use std::process::Child;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// The environment variable that marks every process of a run with that run's id.
pub const RUN_MARKER: &str = "ATELIER_RUN";

/// The environment variable that lists, colon-separated, the run's id after the ids of every run
/// it is nested in (outermost first).
pub const RUNS_MARKER: &str = "ATELIER_RUNS";

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
    /// `pid program arguments` for messages: the program by its file name, not its full path,
    /// then as many arguments as fit in about 160 characters.
    pub fn describe(&self) -> String {
        let (program, args) = match self.command.split_once(' ') {
            Some((p, a)) => (p, a),
            None => (self.command.as_str(), ""),
        };
        let base = program
            .rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or(program);
        let mut text = format!("{} {base}", self.pid);
        if !args.is_empty() {
            text.push(' ');
            text.push_str(args);
        }
        if text.chars().count() > 160 {
            text = text.chars().take(159).collect();
            text.push('…');
        }
        text
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

/// True while the group has a member that is not a zombie. A zombie leader that atelier has not
/// reaped keeps the pgid reserved, so this can be polled while signalling the group safely.
/// When the members cannot be listed, a group [`group_alive`] reports is counted as occupied.
pub fn group_occupied(pgid: i32) -> bool {
    if !group_alive(pgid) {
        return false;
    }
    match group_pids(pgid) {
        Some(members) => members.iter().any(|(_, zombie)| !zombie),
        None => true,
    }
}

/// The processes in a group, for messages; zombies are left out.
pub fn group_members(pgid: i32) -> Vec<ProcInfo> {
    let live: Vec<i32> = group_pids(pgid)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, zombie)| !zombie)
        .map(|(pid, _)| pid)
        .collect();
    list()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.pgid == pgid && live.contains(&p.pid))
        .collect()
}

/// A recorded group is still the one atelier started: the group has a member, and either no
/// process has pid `pgid` (a pgid is never reallocated while its group has members), or that
/// process started at `leader_start`. A different start time means the pgid was reused by
/// something unrelated. Without a recorded start time, a live group counts as ours.
pub fn recorded_group_alive(pgid: i32, leader_start: Option<&str>) -> bool {
    if !group_alive(pgid) {
        return false;
    }
    let Some(recorded) = leader_start else {
        return true;
    };
    #[cfg(target_os = "linux")]
    {
        // Linux start times count from boot: a group recorded in another boot is gone.
        match (recorded.split_once('@'), boot_id()) {
            (Some((_, boot)), Some(now)) if boot != now => return false,
            _ => {}
        }
    }
    match start_time(pgid) {
        Some(now) => now == recorded,
        None => true,
    }
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

/// Stops a group: TERM, wait up to `grace`, KILL, wait. `leader`, the unreaped child whose pid
/// is `pgid`, is reaped only at the end: until then it holds the pgid (as a zombie once it has
/// exited), so every signal sent here reaches this group and nothing else. A leader that has
/// exited does not count as a member. Never touches any other process.
pub fn stop_group(pgid: i32, leader: Option<&mut Child>, grace: Duration) -> Stopped {
    let mut out = Stopped::default();
    if group_occupied(pgid) {
        out.members = group_members(pgid);
        signal_group(pgid, libc::SIGTERM);
        signal_group(pgid, libc::SIGCONT);
        if !wait_empty(pgid, grace) {
            out.killed = true;
            signal_group(pgid, libc::SIGKILL);
            if !wait_empty(pgid, AFTER_KILL) {
                out.remaining = group_members(pgid);
            }
        }
    }
    if let Some(child) = leader {
        if out.remaining.is_empty() {
            let _ = child.wait();
        } else {
            let _ = child.try_wait();
        }
    }
    out
}

fn wait_empty(pgid: i32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if !group_occupied(pgid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(POLL);
    }
}

/// True once the child has exited, without reaping it (`waitid` with `WNOWAIT`): its pid, and
/// so its group's pgid, stay reserved until it is reaped.
pub fn exited(child: &Child) -> std::io::Result<bool> {
    // SAFETY: siginfo_t is plain data; waitid fills it.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: P_PID with our own child's pid; info is a valid out pointer.
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if r == 0 {
            // SAFETY: si_pid is set by waitid; zero means the child has not changed state.
            return Ok(unsafe { siginfo_pid(&info) } != 0);
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

#[cfg(target_os = "linux")]
unsafe fn siginfo_pid(info: &libc::siginfo_t) -> libc::pid_t {
    // SAFETY: the caller passes a siginfo_t filled by waitid.
    unsafe { info.si_pid() }
}

#[cfg(target_os = "macos")]
unsafe fn siginfo_pid(info: &libc::siginfo_t) -> libc::pid_t {
    info.si_pid
}

/// The ids of every run this process is nested in, outermost first: `ATELIER_RUNS` of its own
/// environment, plus `ATELIER_RUN` when an older outer atelier set only that.
pub fn inherited_runs() -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    if let Ok(list) = std::env::var(RUNS_MARKER) {
        for id in list.split(':').filter(|s| !s.is_empty()) {
            if !ids.iter().any(|x| x == id) {
                ids.push(id.to_owned());
            }
        }
    }
    if let Ok(id) = std::env::var(RUN_MARKER)
        && !id.is_empty()
        && !id.contains(':')
        && !ids.contains(&id)
    {
        ids.push(id);
    }
    ids
}

/// True when one environment entry marks the process as belonging to run `id`.
fn entry_marks(entry: &[u8], id: &str) -> bool {
    let id = id.as_bytes();
    if let Some(v) = entry
        .strip_prefix(RUN_MARKER.as_bytes())
        .and_then(|r| r.strip_prefix(b"="))
    {
        return v == id;
    }
    if let Some(v) = entry
        .strip_prefix(RUNS_MARKER.as_bytes())
        .and_then(|r| r.strip_prefix(b"="))
    {
        return v.split(|b| *b == b':').any(|x| x == id);
    }
    false
}

/// Processes, other than this one, marked as belonging to one of `ids`: `ATELIER_RUN=<id>`, or
/// `<id>` among the elements of `ATELIER_RUNS`. Returns each with the first id it matches.
pub fn marker_survivors(ids: &[String]) -> std::io::Result<Vec<(ProcInfo, String)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let me = std::process::id() as i32;
    let mut found = Vec::new();
    for p in list()? {
        if p.pid == me {
            continue;
        }
        let Some(entries) = marker_entries(p.pid) else {
            continue;
        };
        if let Some(id) = ids
            .iter()
            .find(|id| entries.iter().any(|e| entry_marks(e, id)))
        {
            found.push((p, id.clone()));
        }
    }
    Ok(found)
}

/// An opaque token for when a process started, `None` when there is no such process (or it
/// cannot be read). Two equal tokens on one machine mean the same process. macOS:
/// `<seconds>.<microseconds>` of `p_starttime`; Linux: `<clock ticks after boot>@<boot id>`.
#[cfg(target_os = "macos")]
pub fn start_time(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let mut buf = vec![0u8; 1024];
    let mut size = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    // SAFETY: the kernel writes at most `size` bytes into buf and updates size.
    let r = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            4,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 || size < KINFO_MIN {
        return None;
    }
    if i32::from_ne_bytes(buf[KP_PID..KP_PID + 4].try_into().ok()?) != pid {
        return None;
    }
    // kinfo_proc begins with extern_proc, which begins with `struct timeval p_starttime`.
    let secs = i64::from_ne_bytes(buf[0..8].try_into().ok()?);
    let usecs = i32::from_ne_bytes(buf[8..12].try_into().ok()?);
    Some(format!("{secs}.{usecs:06}"))
}

/// Offsets in macOS's `struct kinfo_proc` (LP64): `kp_proc.p_stat` and `kp_proc.p_pid`.
#[cfg(target_os = "macos")]
const KP_STAT: usize = 36;
#[cfg(target_os = "macos")]
const KP_PID: usize = 40;
#[cfg(target_os = "macos")]
const KINFO_MIN: usize = 44;
/// `sizeof(struct kinfo_proc)` on LP64 macOS.
#[cfg(target_os = "macos")]
const KINFO_SIZE: usize = 648;
#[cfg(target_os = "macos")]
const SZOMB: u8 = 5;

/// An opaque token for when a process started, `None` when there is no such process (or it
/// cannot be read). Two equal tokens on one machine mean the same process. macOS:
/// `<seconds>.<microseconds>` of `p_starttime`; Linux: `<clock ticks after boot>@<boot id>`.
#[cfg(target_os = "linux")]
pub fn start_time(pid: i32) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    // Field 22 of stat; index 19 counted from field 3, the first after the command name.
    let ticks = stat[close + 1..].split_whitespace().nth(19)?;
    ticks.parse::<u64>().ok()?;
    Some(format!("{ticks}@{}", boot_id()?))
}

#[cfg(target_os = "linux")]
fn boot_id() -> Option<String> {
    let id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.to_owned())
}

/// The pids in a group, each with whether it is a zombie. `None` when they cannot be listed.
#[cfg(target_os = "macos")]
fn group_pids(pgid: i32) -> Option<Vec<(i32, bool)>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PGRP, pgid];
    for _ in 0..8 {
        let mut size = 0usize;
        // SAFETY: a size query; no buffer is written.
        let r = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                4,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if r != 0 {
            return None;
        }
        size += 8 * KINFO_SIZE;
        let mut buf = vec![0u8; size];
        // SAFETY: the kernel writes at most `size` bytes into buf and updates size.
        let r = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                4,
                buf.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if r != 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOMEM) {
                continue;
            }
            return None;
        }
        return Some(
            buf[..size]
                .as_chunks::<KINFO_SIZE>()
                .0
                .iter()
                .map(|k| {
                    let pid = i32::from_ne_bytes(k[KP_PID..KP_PID + 4].try_into().unwrap());
                    (pid, k[KP_STAT] == SZOMB)
                })
                .collect(),
        );
    }
    None
}

/// The pids in a group, each with whether it is a zombie. `None` when they cannot be listed.
#[cfg(target_os = "linux")]
fn group_pids(pgid: i32) -> Option<Vec<(i32, bool)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()? {
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
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        if fields.get(2).and_then(|s| s.parse::<i32>().ok()) == Some(pgid) {
            out.push((pid, fields.first() == Some(&"Z")));
        }
    }
    Some(out)
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

/// The entries a process's run marker is looked for in: its environment. `None` when the system
/// does not show it.
#[cfg(target_os = "linux")]
pub fn marker_entries(pid: i32) -> Option<Vec<Vec<u8>>> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    Some(
        raw.split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(<[u8]>::to_vec)
            .collect(),
    )
}

/// The entries a process's run marker is looked for in: every string after the executable path,
/// its arguments as well as its environment. Where the arguments end cannot be told reliably
/// (empty arguments look like the padding after the path), and an argument that equals a marker
/// can only add a survivor, never hide one. `None` when the system does not show them (other
/// users' processes, Apple's own programs).
#[cfg(target_os = "macos")]
pub fn marker_entries(pid: i32) -> Option<Vec<Vec<u8>>> {
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

/// Layout: `argc` (native i32), the executable path, NUL padding, the arguments, then the
/// environment entries, each NUL-terminated. Returns every non-empty string after the path.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_procargs2(buf: &[u8]) -> Option<Vec<Vec<u8>>> {
    if buf.len() < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?);
    if argc < 0 {
        return None;
    }
    let rest = &buf[4..];
    let path_end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
    Some(
        rest[path_end..]
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(<[u8]>::to_vec)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    #[test]
    fn procargs2_strings_after_the_path_are_all_searched() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(
            b"/usr/bin/x\0\0\0\0x\0ATELIER_RUN=arg\0A=1\0ATELIER_RUN=r1\0\0junk\0",
        );
        let entries = parse_procargs2(&buf).unwrap();
        assert!(entries.contains(&b"ATELIER_RUN=r1".to_vec()));
        assert!(!entries.contains(&b"/usr/bin/x".to_vec()));
    }

    #[test]
    fn empty_arguments_do_not_hide_the_marker() {
        // argc 3 with two empty arguments: counting arguments would eat the first two
        // environment entries, the marker among them.
        let mut buf = 3i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/usr/bin/x\0\0\0x\0\0\0ATELIER_RUN=r1\0B=2\0\0");
        let entries = parse_procargs2(&buf).unwrap();
        assert!(entries.iter().any(|e| entry_marks(e, "r1")), "{entries:?}");
    }

    #[test]
    fn markers_match_the_run_id_or_an_element_of_the_list() {
        assert!(entry_marks(b"ATELIER_RUN=r1", "r1"));
        assert!(!entry_marks(b"ATELIER_RUN=r10", "r1"));
        assert!(entry_marks(b"ATELIER_RUNS=r0:r1", "r1"));
        assert!(entry_marks(b"ATELIER_RUNS=r1:r2", "r1"));
        assert!(!entry_marks(b"ATELIER_RUNS=r10:r2", "r1"));
        assert!(!entry_marks(b"ATELIER_RUNSX=r1", "r1"));
        assert!(!entry_marks(b"XATELIER_RUN=r1", "r1"));
    }

    #[test]
    fn descriptions_keep_the_program_name_and_arguments() {
        let long = format!(
            "/opt/homebrew/Cellar/python@3.13/3.13.1/Frameworks/Python.framework/Versions/3.13/Resources/Python.app/Contents/MacOS/Python -c {}",
            "x".repeat(300)
        );
        let p = ProcInfo {
            pid: 42,
            pgid: 42,
            command: long,
        };
        let d = p.describe();
        assert!(d.starts_with("42 Python -c xxx"), "{d}");
        assert!(d.chars().count() <= 160, "{d}");
        let p = ProcInfo {
            pid: 7,
            pgid: 7,
            command: "sleep".into(),
        };
        assert_eq!(p.describe(), "7 sleep");
    }

    #[test]
    fn this_process_is_listed_with_its_group() {
        let me = std::process::id() as i32;
        // SAFETY: getpgrp has no preconditions.
        let pgid = unsafe { libc::getpgrp() };
        let p = list().unwrap().into_iter().find(|p| p.pid == me).unwrap();
        assert_eq!(p.pgid, pgid);
        assert!(group_alive(pgid));
        assert!(group_occupied(pgid));
    }

    #[test]
    fn own_environment_is_readable() {
        let env = marker_entries(std::process::id() as i32).expect("own environment");
        assert!(env.iter().any(|e| e.starts_with(b"PATH=")));
    }

    #[test]
    fn start_times_identify_a_process() {
        let me = std::process::id() as i32;
        let a = start_time(me).expect("own start time");
        assert!(!a.contains(' '), "{a}");
        assert_eq!(start_time(me), Some(a));
        // Another user's process (launchd or init) can be read too.
        assert!(start_time(1).is_some());
        assert_eq!(start_time(9_999_999), None);
    }

    fn own_group(program: &str, args: &[&str]) -> Child {
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap()
    }

    #[test]
    fn a_reused_pgid_is_not_the_recorded_group() {
        let mut child = own_group("/bin/sleep", &["30"]);
        let pgid = child.id() as i32;
        let start = start_time(pgid).unwrap();
        assert!(recorded_group_alive(pgid, Some(&start)));
        assert!(recorded_group_alive(pgid, None));
        // Same pgid, another start time: the pgid was reused by something else.
        assert!(!recorded_group_alive(pgid, Some("1.000000")));
        #[cfg(target_os = "linux")]
        {
            let ticks = start.split('@').next().unwrap();
            assert!(!recorded_group_alive(
                pgid,
                Some(&format!("{ticks}@another-boot"))
            ));
        }
        let stopped = stop_group(pgid, Some(&mut child), Duration::from_secs(2));
        assert!(!stopped.members.is_empty());
        assert!(!recorded_group_alive(pgid, Some(&start)));
    }

    #[test]
    fn a_group_whose_leader_is_gone_is_still_recorded_as_ours() {
        // The leader starts a member and exits; the group lives on without a process whose pid
        // is the pgid.
        let mut child = own_group("/bin/sh", &["-c", "sleep 30 & echo $!"]);
        let pgid = child.id() as i32;
        let start = start_time(pgid).unwrap();
        child.wait().unwrap();
        assert!(group_occupied(pgid));
        assert_eq!(start_time(pgid), None);
        assert!(recorded_group_alive(pgid, Some(&start)));
        let stopped = stop_group(pgid, None, Duration::from_secs(2));
        assert_eq!(stopped.members.len(), 1, "{stopped:?}");
        assert!(!group_alive(pgid));
    }

    #[test]
    fn an_exited_leader_is_not_reaped_until_the_group_is_stopped() {
        let mut child = own_group("/bin/sh", &["-c", "sleep 30 & exit 3"]);
        let pgid = child.id() as i32;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !exited(&child).unwrap() {
            assert!(Instant::now() < deadline);
            sleep(POLL);
        }
        // Still unreaped: the pid, and so the pgid, stay reserved.
        assert!(exited(&child).unwrap());
        assert!(group_alive(pgid));
        assert!(group_occupied(pgid), "the sleeper is still a member");
        let members = group_members(pgid);
        assert!(members.iter().all(|m| m.pid != pgid), "{members:?}");
        let stopped = stop_group(pgid, Some(&mut child), Duration::from_secs(2));
        assert_eq!(stopped.members.len(), 1, "{stopped:?}");
        assert_eq!(child.try_wait().unwrap().unwrap().code(), Some(3));
        assert!(!group_alive(pgid));
    }
}
