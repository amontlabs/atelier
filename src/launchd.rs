//! Steps as launchd jobs on macOS.
//!
//! Each step is bootstrapped with a generated plist and a unique label into `gui/<uid>`, or into
//! `user/<uid>` with `LimitLoadToSessionType = Background` when there is no GUI login. The job
//! gets its own resource coalition ([`crate::coalition`]), which every process it starts keeps.
//! `ProcessType = Interactive` keeps launchd from throttling the step; `AbandonProcessGroup`
//! keeps launchd from killing what the step left in its process group, so atelier sees it.
//!
//! The job runs atelier itself as a shim ([`shim_main`]): it opens the step's output files,
//! restores the caller's open-file limit and umask, enters the step's folder, reports its pid,
//! start time and coalition through one FIFO, waits on a second FIFO for atelier's release (sent
//! once the step is recorded under the run's lock; EOF means atelier is gone, and the shim exits
//! 125 without running anything), then replaces itself with the program, with exactly the
//! requested environment (launchd adds its own variables, which the shim drops).
//!
//! atelier waits for the job's pid with `kqueue` (`EVFILT_PROC`, `NOTE_EXIT`) and reads the exit
//! status from `launchctl print`: text, not API, so the preflight probe ([`probe`]) checks it on
//! each macOS version. `launchctl bootout` comes last: it does not stop survivors.
//!
//! Each job's files (the spec, the two FIFOs, the plist and launchd's log) are in a folder of its
//! own under the user's temporary folder (`getconf DARWIN_USER_TEMP_DIR`), never in the project
//! or the state, which may be in a folder macOS protects (`~/Documents`). The folder is named by
//! the job's label and is created, with an `owner` file naming this atelier process, before the
//! job is bootstrapped; it is removed after the `bootout`. A loaded `atelier.*` job whose folder
//! names a dead owner is removed by [`sweep`].

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::coalition::{self, Coalition};
use crate::procs;

/// The first argument that makes atelier the shim of a launchd job. Handled before the
/// dispatcher and the command line.
pub const SHIM_ARG: &str = "__atelier-launchd-step";

/// How long launchd gets to start a job's shim.
const START_WITHIN: Duration = Duration::from_secs(60);

/// How long `launchctl print` gets to report a status after the job's process ended.
const STATUS_WITHIN: Duration = Duration::from_secs(10);

/// The exit status launchd records when it cannot spawn a job's program (`EX_CONFIG`): a
/// program that is missing or cannot be run. The shim itself never exits 78 before it reports.
const SPAWN_FAILED: i32 = 78;

/// The folder of job folders, in the user's temporary folder.
const JOBS_DIR: &str = "atelier-jobs";

/// The file in a job folder that names the atelier process that started the job.
const OWNER_FILE: &str = "owner";

/// The user's own temporary folder (`confstr(_CS_DARWIN_USER_TEMP_DIR)`), whatever `TMPDIR` a
/// step set; `std::env::temp_dir()` when it cannot be read.
pub fn user_temp_dir() -> PathBuf {
    let mut buf = vec![0u8; 1024];
    // SAFETY: confstr writes at most `buf.len()` bytes, NUL included.
    let n = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    if n == 0 || n > buf.len() {
        return std::env::temp_dir();
    }
    buf.truncate(n - 1);
    PathBuf::from(OsString::from_vec(buf))
}

/// Where the job folders are: `<user temp>/atelier-jobs/`.
pub fn jobs_root() -> PathBuf {
    user_temp_dir().join(JOBS_DIR)
}

/// The folder of the job with `label`.
pub fn job_dir(label: &str) -> PathBuf {
    jobs_root().join(label)
}

/// What a step job runs, and where.
#[derive(Debug, Clone)]
pub struct Spec {
    pub argv: Vec<OsString>,
    pub cwd: PathBuf,
    /// The step's whole environment, exactly.
    pub env: Vec<(OsString, OsString)>,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
}

/// Why a job did not start its program.
#[derive(Debug)]
pub enum LaunchError {
    /// launchd jobs cannot be started here at all.
    Unavailable(String),
    /// This job did not start: the reason goes to the step's log.
    NotStarted(String),
    /// A stop signal arrived before the job reported.
    Interrupted,
}

/// A started step job, its program not yet released.
#[derive(Debug)]
pub struct Job {
    pub label: String,
    target: String,
    dir: PathBuf,
    /// The job's process: the shim, then the step's program (same pid); its process group.
    pub pid: i32,
    /// [`procs::start_time`] of `pid`.
    pub start: Option<String>,
    pub coalition: Coalition,
    go: Option<File>,
    booted_out: bool,
}

static PROGRAM: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Makes `path` the program step jobs run as their shim, for a process that is not atelier
/// itself (a test binary using the library). The first call wins.
#[doc(hidden)]
pub fn use_program(path: impl Into<PathBuf>) {
    let _ = PROGRAM.set(path.into());
}

/// The atelier program the job runs as its shim: this program, unless [`use_program`] named
/// another. A program that is not named `atelier` cannot be the shim (it would not know the
/// shim's argument).
pub fn program() -> std::io::Result<PathBuf> {
    if let Some(p) = PROGRAM.get() {
        return Ok(p.clone());
    }
    let exe = std::env::current_exe()?;
    if exe.file_name() == Some(OsStr::new("atelier")) {
        return Ok(exe);
    }
    Err(std::io::Error::other(format!(
        "{} is not atelier, so it cannot start step jobs",
        exe.display()
    )))
}

fn uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

/// 0 unknown, 1 `gui/<uid>`, 2 `user/<uid>`: the domain that worked last in this process.
static DOMAIN: AtomicU8 = AtomicU8::new(0);

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn utf8<'a>(p: &'a Path, what: &str) -> Result<&'a str, LaunchError> {
    p.to_str()
        .ok_or_else(|| LaunchError::NotStarted(format!("{what} is not UTF-8: {}", p.display())))
}

fn plist(label: &str, program: &str, spec: &str, log: &str, background: bool) -> String {
    let mut p = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n",
    );
    let kv = |k: &str, v: &str| format!("<key>{k}</key><string>{}</string>\n", xml(v));
    p += &kv("Label", label);
    p += "<key>ProgramArguments</key><array>";
    for a in [program, SHIM_ARG, spec] {
        p += &format!("<string>{}</string>", xml(a));
    }
    p += "</array>\n";
    p += &kv("WorkingDirectory", "/");
    p += &kv("StandardInPath", "/dev/null");
    p += &kv("StandardOutPath", log);
    p += &kv("StandardErrorPath", log);
    p += &kv("ProcessType", "Interactive");
    if background {
        p += &kv("LimitLoadToSessionType", "Background");
    }
    p += "<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><false/>\n";
    p += "<key>AbandonProcessGroup</key><true/>\n</dict>\n</plist>\n";
    p
}

/// The spec file: NUL-terminated key, value pairs.
fn spec_bytes(spec: &Spec, ready: &Path, go: &Path, probe: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut push = |k: &str, v: &[u8]| {
        out.extend_from_slice(k.as_bytes());
        out.push(0);
        out.extend_from_slice(v);
        out.push(0);
    };
    push("ready", ready.as_os_str().as_bytes());
    push("go", go.as_os_str().as_bytes());
    push("out", spec.stdout.as_os_str().as_bytes());
    push("err", spec.stderr.as_os_str().as_bytes());
    push("cwd", spec.cwd.as_os_str().as_bytes());
    let mut rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: rl is a valid out pointer.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } == 0 {
        push("nofile", rl.rlim_cur.to_string().as_bytes());
    }
    // SAFETY: umask only sets the mask; it is put back at once.
    let mask = unsafe {
        let m = libc::umask(0o022);
        libc::umask(m);
        m
    };
    push("umask", mask.to_string().as_bytes());
    for (k, v) in &spec.env {
        let mut e = k.as_bytes().to_vec();
        e.push(b'=');
        e.extend_from_slice(v.as_bytes());
        push("env", &e);
    }
    for a in &spec.argv {
        push("arg", a.as_bytes());
    }
    if probe {
        push("probe", b"1");
    }
    out
}

fn mkfifo(path: &Path) -> std::io::Result<()> {
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("a NUL in the path"))?;
    // SAFETY: c is a valid C string.
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn set_blocking(f: &File) {
    // SAFETY: fcntl on a descriptor this File owns.
    unsafe {
        let flags = libc::fcntl(f.as_raw_fd(), libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(f.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }
}

fn launchctl(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("/bin/launchctl")
        .args(args)
        .stdin(Stdio::null())
        .output()
}

/// What `launchctl print` says about a job.
#[derive(Debug, Default, PartialEq, Eq)]
struct Print {
    running: bool,
    exit: Option<i32>,
    signal: Option<i32>,
    /// `last exit code` as printed, `78: EX_CONFIG`.
    exit_text: Option<String>,
}

fn parse_print(text: &str) -> Print {
    let mut p = Print::default();
    for l in text.lines().map(str::trim) {
        if l == "state = running" {
            p.running = true;
        }
        if let Some(v) = l.strip_prefix("last exit code = ") {
            // `7`, or with a sysexits name for 64 to 78: `78: EX_CONFIG`.
            p.exit = v.split(':').next().and_then(|n| n.trim().parse().ok());
            p.exit_text = Some(v.trim().to_owned());
        }
        if let Some(v) = l.strip_prefix("last terminating signal = ") {
            // `Terminated: 15`
            p.signal = v.rsplit(": ").next().and_then(|n| n.trim().parse().ok());
        }
    }
    p
}

fn print_job(target: &str) -> Option<Print> {
    let out = launchctl(&["print", target]).ok()?;
    out.status
        .success()
        .then(|| parse_print(&String::from_utf8_lossy(&out.stdout)))
}

/// Starts a step job with `label` and waits until its shim reports, its program not yet
/// released. `interrupted` is polled while waiting. The job's folder ([`job_dir`]) must not
/// exist yet.
pub fn launch(
    label: &str,
    spec: &Spec,
    interrupted: &mut dyn FnMut() -> bool,
) -> Result<Job, LaunchError> {
    launch_inner(label, spec, false, interrupted)
}

/// Removes a job folder unless defused: the folder of a job that was never bootstrapped.
struct DirGuard(Option<PathBuf>);

impl Drop for DirGuard {
    fn drop(&mut self) {
        if let Some(d) = self.0.take() {
            let _ = fs::remove_dir_all(d);
        }
    }
}

/// Why launchd could not spawn the shim, from what the program path is.
fn spawn_failure(program: &str, exit_text: Option<&str>) -> String {
    let path = Path::new(program);
    let why = match fs::metadata(path) {
        Err(e) => format!("{program}: {e}"),
        Ok(m) if !m.is_file() => format!("{program} is not a file"),
        Ok(m) if std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o111 == 0 => {
            format!("{program} is not executable")
        }
        Ok(_) => format!("{program} cannot be run"),
    };
    format!(
        "launchd could not start the shim: {why} (launchctl print: last exit code = {})",
        exit_text.unwrap_or("78")
    )
}

fn launch_inner(
    label: &str,
    spec: &Spec,
    probe: bool,
    interrupted: &mut dyn FnMut() -> bool,
) -> Result<Job, LaunchError> {
    let io = |what: &str, e: std::io::Error| LaunchError::NotStarted(format!("{what}: {e}"));
    let program =
        program().map_err(|e| LaunchError::Unavailable(format!("the job's program: {e}")))?;
    let boot = coalition::boot_session().ok_or_else(|| {
        LaunchError::Unavailable("cannot read this boot's session id (kern.bootsessionuuid)".into())
    })?;
    let dir = job_dir(label);
    fs::create_dir_all(jobs_root()).map_err(|e| io("the job folders", e))?;
    fs::create_dir(&dir).map_err(|e| io(&format!("{}", dir.display()), e))?;
    let mut guard = DirGuard(Some(dir.clone()));
    // The intent record: written before the bootstrap, so a job this process leaves loaded when
    // it dies is found by `sweep`.
    let me = std::process::id() as i32;
    fs::write(
        dir.join(OWNER_FILE),
        format!(
            "{me} {}\n",
            procs::start_time(me).unwrap_or_else(|| "-".into())
        ),
    )
    .map_err(|e| io("the job's owner file", e))?;
    let (ready_path, go_path) = (dir.join("ready"), dir.join("go"));
    let (spec_path, plist_path, log_path) = (
        dir.join("spec"),
        dir.join("job.plist"),
        dir.join("launchd.log"),
    );
    fs::write(&spec_path, spec_bytes(spec, &ready_path, &go_path, probe))
        .map_err(|e| io("the job's spec", e))?;
    mkfifo(&ready_path).map_err(|e| io("the job's FIFO", e))?;
    mkfifo(&go_path).map_err(|e| io("the job's FIFO", e))?;
    let mut ready = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&ready_path)
        .map_err(|e| io("the job's FIFO", e))?;
    // A writer needs a reader to open without blocking: open one, then let it go. The shim
    // opens its reader before it reports, so "go" is never written without one.
    let go = {
        let _reader = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&go_path)
            .map_err(|e| io("the job's FIFO", e))?;
        OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&go_path)
            .map_err(|e| io("the job's FIFO", e))?
    };

    let program = utf8(&program, "the atelier program")?.to_owned();
    let spec_s = utf8(&spec_path, "the job folder")?.to_owned();
    let log_s = utf8(&log_path, "the job folder")?.to_owned();
    let uid = uid();
    let order: &[u8] = match DOMAIN.load(Ordering::Relaxed) {
        2 => &[2, 1],
        _ => &[1, 2],
    };
    let mut errors = Vec::new();
    let mut target = None;
    for d in order {
        let (domain, background) = match d {
            1 => (format!("gui/{uid}"), false),
            _ => (format!("user/{uid}"), true),
        };
        fs::write(
            &plist_path,
            plist(label, &program, &spec_s, &log_s, background),
        )
        .map_err(|e| io("the job's plist", e))?;
        let plist_s = utf8(&plist_path, "the job folder")?;
        match launchctl(&["bootstrap", &domain, plist_s]) {
            Ok(out) if out.status.success() => {
                DOMAIN.store(*d, Ordering::Relaxed);
                target = Some(format!("{domain}/{label}"));
                break;
            }
            Ok(out) => errors.push(format!(
                "launchctl bootstrap {domain}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => errors.push(format!("launchctl bootstrap {domain}: {e}")),
        }
    }
    let Some(target) = target else {
        return Err(LaunchError::Unavailable(errors.join("; ")));
    };
    // From here the job's bootout removes the folder.
    guard.0 = None;
    let mut job = Job {
        label: label.to_owned(),
        target,
        dir,
        pid: 0,
        start: None,
        coalition: Coalition { id: 0, boot },
        go: Some(go),
        booted_out: false,
    };

    let started = Instant::now();
    let mut last_print = Instant::now();
    let mut line = Vec::new();
    loop {
        let mut buf = [0u8; 512];
        match ready.read(&mut buf) {
            Ok(n) if n > 0 => line.extend_from_slice(&buf[..n]),
            _ => {}
        }
        if line.ends_with(b"\n") {
            break;
        }
        if interrupted() {
            return Err(LaunchError::Interrupted);
        }
        if last_print.elapsed() > Duration::from_millis(250) {
            last_print = Instant::now();
            if let Some(p) = print_job(&job.target)
                && !p.running
                && (p.exit.is_some() || p.signal.is_some())
            {
                // Ended without reporting; read once more in case the report raced the exit.
                if let Ok(n) = ready.read(&mut buf) {
                    line.extend_from_slice(&buf[..n]);
                }
                if line.ends_with(b"\n") {
                    break;
                }
                if p.exit == Some(SPAWN_FAILED) && p.signal.is_none() {
                    // launchd could not spawn the shim at all (`spawn scheduled`, EX_CONFIG):
                    // no step can start with this program.
                    return Err(LaunchError::Unavailable(spawn_failure(
                        &program,
                        p.exit_text.as_deref(),
                    )));
                }
                let log = fs::read_to_string(&log_path).unwrap_or_default();
                return Err(LaunchError::NotStarted(format!(
                    "the step's launchd job ended before it started the program ({}){}",
                    describe_status(p.exit, p.signal),
                    if log.trim().is_empty() {
                        String::new()
                    } else {
                        format!(": {}", log.trim())
                    }
                )));
            }
        }
        if started.elapsed() > START_WITHIN {
            return Err(LaunchError::NotStarted(format!(
                "launchd did not start the step's job within {} s",
                START_WITHIN.as_secs()
            )));
        }
        sleep(Duration::from_millis(2));
    }
    let text = String::from_utf8_lossy(&line).trim().to_owned();
    let fields: Vec<&str> = text.split(' ').collect();
    match fields.as_slice() {
        ["ok", pid, start, coal] => {
            job.pid = pid.parse().unwrap_or(0);
            job.start = (*start != "-").then(|| (*start).to_owned());
            job.coalition.id = coal.parse().unwrap_or(0);
            if job.pid <= 1 || job.coalition.id == 0 {
                return Err(LaunchError::Unavailable(format!(
                    "the job's shim reported no coalition ({text})"
                )));
            }
            Ok(job)
        }
        _ => Err(LaunchError::NotStarted(
            text.strip_prefix("error ").unwrap_or(&text).to_owned(),
        )),
    }
}

fn describe_status(exit: Option<i32>, signal: Option<i32>) -> String {
    match (exit, signal) {
        (_, Some(s)) => format!("signal {s}"),
        (Some(c), None) => format!("exit {c}"),
        (None, None) => "no status".into(),
    }
}

impl Job {
    /// The job's folder ([`job_dir`]), removed by [`Job::bootout`].
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Lets the shim run the program. False when the shim is gone.
    pub fn release(&mut self) -> bool {
        match self.go.take() {
            Some(mut go) => {
                set_blocking(&go);
                go.write_all(b"go\n").is_ok()
            }
            None => false,
        }
    }

    /// Closes the release FIFO unused: the shim exits 125 without running anything.
    pub fn abandon(&mut self) {
        self.go = None;
    }

    /// Waits for the job's process to end. Returns false when `interrupted` says so first.
    pub fn wait(&self, interrupted: &mut dyn FnMut() -> bool) -> std::io::Result<bool> {
        // SAFETY: kqueue returns a new descriptor or -1.
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: kq is a descriptor this function owns from here on.
        let kq = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(kq) };
        // SAFETY: kevent is plain data.
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = self.pid as usize;
        ev.filter = libc::EVFILT_PROC;
        ev.flags = libc::EV_ADD | libc::EV_ONESHOT;
        ev.fflags = libc::NOTE_EXIT;
        // SAFETY: one change, no events out.
        let r = unsafe {
            libc::kevent(
                kq.as_raw_fd(),
                &ev,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if r < 0 {
            // ESRCH: already gone.
            return Ok(true);
        }
        // The pid could have ended and been reused before the registration.
        if self.start.is_some() && procs::start_time(self.pid) != self.start {
            return Ok(true);
        }
        let ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 20_000_000,
        };
        loop {
            // SAFETY: kevent is plain data.
            let mut out: libc::kevent = unsafe { std::mem::zeroed() };
            // SAFETY: no changes, one event out, a valid timeout.
            let n = unsafe { libc::kevent(kq.as_raw_fd(), std::ptr::null(), 0, &mut out, 1, &ts) };
            if n > 0 {
                return Ok(true);
            }
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
            if interrupted() {
                return Ok(false);
            }
        }
    }

    /// The program's exit code and terminating signal, from `launchctl print`, once the job's
    /// process has ended.
    pub fn status(&self) -> (Option<i32>, Option<i32>) {
        let deadline = Instant::now() + STATUS_WITHIN;
        loop {
            if let Some(p) = print_job(&self.target)
                && !p.running
                && (p.exit.is_some() || p.signal.is_some())
            {
                return match p.signal {
                    Some(s) => (None, Some(s)),
                    None => (p.exit, None),
                };
            }
            if Instant::now() >= deadline {
                return (None, None);
            }
            sleep(Duration::from_millis(2));
        }
    }

    /// Removes the job from launchd and its files from the run folder; launchd's own messages,
    /// if any, are returned.
    pub fn bootout(&mut self) -> Option<String> {
        self.go = None;
        if self.booted_out {
            return None;
        }
        self.booted_out = true;
        let mut notes = Vec::new();
        match launchctl(&["bootout", &self.target]) {
            Ok(out) if out.status.success() => {}
            Ok(out) => notes.push(format!(
                "launchctl bootout {}: {}",
                self.target,
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => notes.push(format!("launchctl bootout {}: {e}", self.target)),
        }
        if let Ok(log) = fs::read_to_string(self.dir.join("launchd.log"))
            && !log.trim().is_empty()
        {
            notes.push(format!("launchd: {}", log.trim()));
        }
        let _ = fs::remove_dir_all(&self.dir);
        (!notes.is_empty()).then(|| notes.join("\n"))
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        self.bootout();
    }
}

/// Removes a job left loaded by an atelier that died (its label from a run's coalitions file),
/// in whichever domain it is, and its folder. Errors are ignored: the job is usually gone.
pub fn bootout_label(label: &str) {
    let uid = uid();
    for d in ["gui", "user"] {
        let _ = launchctl(&["bootout", &format!("{d}/{uid}/{label}")]);
    }
    if !label.is_empty() && !label.contains('/') && label.starts_with("atelier.") {
        let _ = fs::remove_dir_all(job_dir(label));
    }
}

/// The `atelier.*` jobs loaded in this user's `gui` and `user` domains, each with whether its
/// process runs, from the `services` lists of `launchctl print` (`<pid> <status> <label>`, pid 0
/// when not running).
fn loaded_jobs() -> Vec<(String, bool)> {
    let uid = uid();
    let mut out: Vec<(String, bool)> = Vec::new();
    for d in ["gui", "user"] {
        let Ok(o) = launchctl(&["print", &format!("{d}/{uid}")]) else {
            continue;
        };
        if !o.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&o.stdout);
        let mut inside = false;
        for l in text.lines() {
            let t = l.trim();
            if t == "services = {" {
                inside = true;
                continue;
            }
            if inside && t == "}" {
                break;
            }
            if !inside {
                continue;
            }
            let f: Vec<&str> = t.split_whitespace().collect();
            if let [pid, .., label] = f.as_slice()
                && label.starts_with("atelier.")
                && !out.iter().any(|(l, _)| l == label)
            {
                out.push(((*label).to_owned(), pid.parse::<i32>().is_ok_and(|p| p > 0)));
            }
        }
    }
    out
}

/// What the owner file of a job folder says: `Some(true)` when the atelier process it names is
/// alive, `Some(false)` when it is gone, `None` when there is no readable owner file.
fn owner_alive(dir: &Path) -> Option<bool> {
    let text = fs::read_to_string(dir.join(OWNER_FILE)).ok()?;
    let mut it = text.split_whitespace();
    let pid: i32 = it.next()?.parse().ok()?;
    let start = it.next().filter(|s| *s != "-");
    if pid <= 1 {
        return None;
    }
    let now = procs::start_time(pid);
    Some(match start {
        Some(s) => now.as_deref() == Some(s),
        // SAFETY: an existence check only.
        None => now.is_some() || unsafe { libc::kill(pid, 0) } == 0,
    })
}

/// Removes the jobs atelier processes that died left behind: every loaded `atelier.*` job that
/// does not run and whose folder names a dead owner; a probe job (`atelier.probe.<pid>.<hex>`)
/// whose pid is gone; and a job of a run `dead_run` says is dead (`atelier.<run id>.<n>`).
/// Folders of dead owners whose job is not loaded go too. A job whose process still runs is left
/// alone: what it runs is a survivor, found by coalition or marker, and the next sweep after it
/// ends removes it. Returns the labels booted out.
pub fn sweep(dead_run: &dyn Fn(&str) -> bool) -> Vec<String> {
    let mut removed = Vec::new();
    let loaded = loaded_jobs();
    for (label, running) in &loaded {
        if *running {
            continue;
        }
        let dead = match owner_alive(&job_dir(label)) {
            Some(alive) => !alive,
            None => {
                let rest = &label["atelier.".len()..];
                if let Some(p) = rest.strip_prefix("probe.") {
                    p.split('.')
                        .next()
                        .and_then(|p| p.parse::<i32>().ok())
                        .filter(|p| *p > 1)
                        // SAFETY: an existence check only.
                        .is_some_and(|p| unsafe { libc::kill(p, 0) } != 0)
                } else {
                    rest.rsplit_once('.')
                        .is_some_and(|(id, n)| n.parse::<u32>().is_ok() && dead_run(id))
                }
            }
        };
        if dead {
            bootout_label(label);
            removed.push(label.clone());
        }
    }
    if let Ok(entries) = fs::read_dir(jobs_root()) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if loaded.iter().any(|(l, _)| *l == name) {
                continue;
            }
            if owner_alive(&e.path()) == Some(false) {
                let _ = fs::remove_dir_all(e.path());
            }
        }
    }
    removed
}

/// The preflight's probe: a job whose program starts a sleeper in a new session with a double
/// fork and an empty environment, then exits 3. Passes when `launchctl print` reports exit 3
/// and the sleeper is found by the job's coalition and stopped.
pub fn probe() -> Result<(), String> {
    let label = format!(
        "atelier.probe.{}.{}",
        std::process::id(),
        crate::fsutil::random_hex(4).map_err(|e| e.to_string())?
    );
    let log = job_dir(&label).join("probe.log");
    let spec = Spec {
        argv: vec!["/bin/sleep".into(), "30".into()],
        cwd: PathBuf::from("/"),
        env: Vec::new(),
        stdout: log.clone(),
        stderr: log,
    };
    let mut job = match launch_inner(&label, &spec, true, &mut || false) {
        Ok(j) => j,
        Err(LaunchError::Unavailable(m) | LaunchError::NotStarted(m)) => {
            return Err(format!("launchd jobs cannot be started: {m}"));
        }
        Err(LaunchError::Interrupted) => return Err("interrupted".into()),
    };
    let ids = [job.coalition.clone()];
    if !job.release() {
        return Err("the probe job ended before it was released".into());
    }
    job.wait(&mut || false).map_err(|e| e.to_string())?;
    let (exit, signal) = job.status();
    let found = coalition::members(&ids);
    let stopped = coalition::stop(&ids, Duration::from_secs(2));
    let notes = job.bootout();
    if exit != Some(3) {
        return Err(format!(
            "launchctl print did not report the probe's exit status 3 (got {}){}",
            describe_status(exit, signal),
            notes.map(|n| format!("; {n}")).unwrap_or_default()
        ));
    }
    if found.is_empty() {
        return Err(
            "a process that left its session (setsid and a double fork) was not in the job's coalition"
                .into(),
        );
    }
    if !stopped.remaining.is_empty() {
        return Err("the probe's sleeper could not be stopped".into());
    }
    Ok(())
}

// ----------------------------------------------------------------------------- the shim

struct ShimSpec {
    ready: PathBuf,
    go: PathBuf,
    out: PathBuf,
    err: PathBuf,
    cwd: PathBuf,
    nofile: Option<u64>,
    umask: Option<libc::mode_t>,
    env: Vec<(OsString, OsString)>,
    argv: Vec<OsString>,
    probe: bool,
}

fn read_spec(path: &Path) -> std::io::Result<ShimSpec> {
    let data = fs::read(path)?;
    let parts: Vec<&[u8]> = data.split(|b| *b == 0).collect();
    let mut s = ShimSpec {
        ready: PathBuf::new(),
        go: PathBuf::new(),
        out: PathBuf::new(),
        err: PathBuf::new(),
        cwd: PathBuf::from("/"),
        nofile: None,
        umask: None,
        env: Vec::new(),
        argv: Vec::new(),
        probe: false,
    };
    let os = |b: &[u8]| OsString::from_vec(b.to_vec());
    for [k, v] in parts.as_chunks::<2>().0 {
        let (k, v) = (*k, *v);
        match k {
            b"ready" => s.ready = os(v).into(),
            b"go" => s.go = os(v).into(),
            b"out" => s.out = os(v).into(),
            b"err" => s.err = os(v).into(),
            b"cwd" => s.cwd = os(v).into(),
            b"nofile" => s.nofile = std::str::from_utf8(v).ok().and_then(|t| t.parse().ok()),
            b"umask" => s.umask = std::str::from_utf8(v).ok().and_then(|t| t.parse().ok()),
            b"env" => {
                let i = v.iter().position(|b| *b == b'=').unwrap_or(v.len());
                s.env
                    .push((os(&v[..i]), os(v.get(i + 1..).unwrap_or_default())));
            }
            b"arg" => s.argv.push(os(v)),
            b"probe" => s.probe = true,
            _ => {}
        }
    }
    Ok(s)
}

fn redirect(path: &Path, fd: libc::c_int) -> std::io::Result<()> {
    let f = OpenOptions::new().create(true).append(true).open(path)?;
    // SAFETY: dup2 onto a standard descriptor; f stays open until after the call.
    if unsafe { libc::dup2(f.as_raw_fd(), fd) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The job's entry point (`atelier __atelier-launchd-step <spec>`). Returns only on failure,
/// with the exit code: 125 when the program was never run.
pub fn shim_main(args: &[OsString]) -> i32 {
    let Some(spec_path) = args.get(2) else {
        eprintln!("atelier: {SHIM_ARG} needs a spec file");
        return 125;
    };
    let spec = match read_spec(Path::new(spec_path)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("atelier: {}: {e}", Path::new(spec_path).display());
            return 125;
        }
    };
    let program = spec
        .argv
        .first()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Errors before the report go to launchd's log and to the report.
    let fail = |msg: String| -> i32 {
        eprintln!("atelier: {msg}");
        if let Ok(mut f) = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&spec.ready)
        {
            let _ = f.write_all(format!("error {}\n", msg.replace('\n', " ")).as_bytes());
        }
        125
    };
    if let Err(e) = redirect(&spec.out, 1) {
        return fail(format!("{}: {e}", spec.out.display()));
    }
    if let Err(e) = redirect(&spec.err, 2) {
        return fail(format!("{}: {e}", spec.err.display()));
    }
    if let Some(n) = spec.nofile {
        let mut rl = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: rl is a valid in/out pointer.
        unsafe {
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) == 0 {
                rl.rlim_cur = n.min(rl.rlim_max);
                libc::setrlimit(libc::RLIMIT_NOFILE, &rl);
            }
        }
    }
    if let Some(m) = spec.umask {
        // SAFETY: sets this process's file mode mask.
        unsafe { libc::umask(m) };
    }
    if let Err(e) = std::env::set_current_dir(&spec.cwd) {
        return fail(format!(
            "cannot start {program}: {}: {e}",
            spec.cwd.display()
        ));
    }
    let go = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&spec.go)
    {
        Ok(f) => f,
        Err(e) => return fail(format!("{}: {e}", spec.go.display())),
    };
    set_blocking(&go);
    let me = std::process::id() as i32;
    let report = format!(
        "ok {me} {} {}\n",
        procs::start_time(me).unwrap_or_else(|| "-".into()),
        coalition::of(me).unwrap_or(0)
    );
    match OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&spec.ready)
    {
        Ok(mut f) => {
            if f.write_all(report.as_bytes()).is_err() {
                return 125;
            }
        }
        // No reader: atelier is gone.
        Err(_) => return 125,
    }
    let mut buf = Vec::new();
    let mut go = go;
    let mut byte = [0u8; 1];
    while let Ok(1) = go.read(&mut byte) {
        buf.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    if buf != b"go\n" {
        return 125;
    }
    drop(go);
    if spec.probe {
        return probe_child();
    }
    let Some((prog, rest)) = spec.argv.split_first() else {
        eprintln!("atelier: empty command");
        return 125;
    };
    let err = Command::new(prog)
        .args(rest)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .exec();
    eprintln!("atelier: cannot run {}: {err}", prog.to_string_lossy());
    if err.kind() == std::io::ErrorKind::NotFound {
        127
    } else {
        126
    }
}

/// The probe's program: a sleeper in a new session after a double fork, with an empty
/// environment; then exit 3.
fn probe_child() -> i32 {
    // SAFETY: the shim is single-threaded; the children only call async-signal-safe functions
    // before exec or _exit.
    unsafe {
        let child = libc::fork();
        if child < 0 {
            return 125;
        }
        if child == 0 {
            libc::setsid();
            if libc::fork() == 0 {
                let prog = c"/bin/sleep";
                let arg = c"30";
                let argv = [prog.as_ptr(), arg.as_ptr(), std::ptr::null()];
                let envp: [*const libc::c_char; 1] = [std::ptr::null()];
                libc::execve(prog.as_ptr(), argv.as_ptr(), envp.as_ptr());
            }
            libc::_exit(0);
        }
        let mut status = 0;
        libc::waitpid(child, &mut status, 0);
    }
    3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchctl_print_is_read() {
        let text = "gui/501/x = {\n\tstate = not running\n\tlast exit code = 7\n}\n";
        assert_eq!(
            parse_print(text),
            Print {
                running: false,
                exit: Some(7),
                signal: None,
                exit_text: Some("7".into()),
            }
        );
        let text = "\tstate = running\n\tlast exit code = (never exited)\n";
        assert_eq!(
            parse_print(text),
            Print {
                running: true,
                exit: None,
                signal: None,
                exit_text: Some("(never exited)".into()),
            }
        );
        let text = "\tstate = not running\n\tlast terminating signal = Terminated: 15\n";
        assert_eq!(parse_print(text).signal, Some(15));
        // launchd names the sysexits codes 64 to 78.
        for (line, code) in [
            ("last exit code = 64: EX_USAGE", 64),
            ("last exit code = 70: EX_SOFTWARE", 70),
            ("last exit code = 78: EX_CONFIG", 78),
            ("last exit code = 255", 255),
        ] {
            let p = parse_print(&format!("\tstate = spawn scheduled\n\t{line}\n"));
            assert_eq!((p.running, p.exit), (false, Some(code)), "{line}");
        }
    }

    #[test]
    fn plists_escape_and_name_the_domain_keys() {
        let p = plist("atelier.x.1", "/a&b/atelier", "/s<p>ec", "/l", true);
        assert!(p.contains("<string>/a&amp;b/atelier</string>"), "{p}");
        assert!(p.contains("<string>/s&lt;p&gt;ec</string>"), "{p}");
        assert!(p.contains("<key>LimitLoadToSessionType</key><string>Background</string>"));
        assert!(p.contains("<key>ProcessType</key><string>Interactive</string>"));
        assert!(p.contains("<key>AbandonProcessGroup</key><true/>"));
        assert!(!plist("l", "/p", "/s", "/l", false).contains("LimitLoadToSessionType"));
    }

    #[test]
    fn specs_keep_bytes_and_empty_values() {
        let dir = tempfile::tempdir().unwrap();
        let spec = Spec {
            argv: vec!["/bin/echo".into(), "a b".into(), "".into()],
            cwd: "/tmp".into(),
            env: vec![
                ("EMPTY".into(), "".into()),
                ("EQ".into(), "a=b".into()),
                ("RAW".into(), OsString::from_vec(vec![0xff, b'x'])),
            ],
            stdout: "/o".into(),
            stderr: "/e".into(),
        };
        let path = dir.path().join("spec");
        fs::write(
            &path,
            spec_bytes(&spec, Path::new("/r"), Path::new("/g"), false),
        )
        .unwrap();
        let s = read_spec(&path).unwrap();
        assert_eq!(s.argv, spec.argv);
        assert_eq!(s.env, spec.env);
        assert_eq!(s.cwd, PathBuf::from("/tmp"));
        assert!(!s.probe);
        assert!(s.nofile.is_some());
    }
}
