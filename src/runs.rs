//! Runs (r4 §3.1): every run has its own id and folder `<state>/runs/<run id>/`, with its logs,
//! `run.toml` written when it starts and one `result` written atomically when it ends, naming the
//! exact tree it tested. A folder without `result` is a run that ended without a result. Both
//! files are frozen formats (`docs/FROZEN-LAYER.md`): readers ignore fields they do not know.
//!
//! A run holds one lock for its whole life. Each step is started as the leader of its own
//! process group, with `ATELIER_RUN=<run id>` and `ATELIER_RUNS=<outer run ids>:<run id>` set
//! last in its environment, so no step environment can replace them. Its group, with the
//! leader's start time, is recorded under the lock before the step's program runs (a `/bin/sh`
//! trampoline waits for atelier's release). When the leader ends, a group that still has members
//! is stopped (TERM, wait, KILL) and the step fails with "left processes behind"; the leader is
//! reaped only after that, so its pgid cannot be reused while it is signalled. When the run ends,
//! the marker scan looks for processes that left the group; any found makes the run fail, is
//! named, and is not signalled. A TERM, INT or HUP during a run stops the current group and the
//! run ends with a result that says so.
//!
//! On macOS each step is a launchd job instead (DESIGN §8.4, [`crate::launchd`]): the job's
//! process is the group leader, started by atelier's shim with exactly the step's environment,
//! and held until its group and coalition are recorded under the lock. When it ends, every
//! process of its coalition still alive after [`crate::coalition::SETTLE`] (those that left the
//! group, the session or the environment included) is stopped and the step fails with "left
//! processes behind". The run's coalitions,
//! and those of runs nested in it, are listed in the run folder; the end-of-run scan looks
//! through them instead of the markers, stops what it finds, and the run fails. A run refuses
//! (4) to start on a macOS where the preflight probe ([`crate::coalition::ensure_probed`])
//! fails: there is no fallback to the marker scan.

use std::fs::{self, File};
use std::io::{ErrorKind, Write};
#[cfg(not(target_os = "macos"))]
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
#[cfg(not(target_os = "macos"))]
use std::process::{Command, Stdio};
#[cfg(not(target_os = "macos"))]
use std::thread::sleep;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::dispatch::{DISPATCHED_VAR, VERSION};
use crate::exit::{Failure, IoContext, Result};
use crate::fsutil;
use crate::git;
use crate::locks::{HeldLock, LockKey};
use crate::names;
use crate::platform::Platform;
use crate::procs::{self, RUN_MARKER, RUNS_MARKER};
use crate::signals;
use crate::state::State;

/// Waits for one line on stdin (atelier's release, sent once the group is recorded), then runs
/// the step's program with stdin from /dev/null. EOF without a line: exit 125, nothing run.
pub const STEP_TRAMPOLINE: &str = "read _ || exit 125; exec \"$@\" </dev/null";

/// Random bytes in a run id. Ids must not collide across every clone of one user on a machine:
/// the marker scan covers the whole machine.
const RUN_ID_RANDOM_BYTES: usize = 8;

/// The id the next `atelier check` takes for its run, when free: how the gate knows its inner
/// check's run exactly. Only a step given it explicitly sees it.
pub const RUN_ID_VAR: &str = "ATELIER_RUN_ID";

/// A new run id: UTC time to the minute plus 8 random bytes, `20261113T1012Z-4f2a09c1d3e5b7a8`.
pub fn new_run_id() -> Result<String> {
    let (y, mo, d, h, mi, _) = fsutil::utc_parts(fsutil::now_secs());
    Ok(format!(
        "{y:04}{mo:02}{d:02}T{h:02}{mi:02}Z-{}",
        fsutil::random_hex(RUN_ID_RANDOM_BYTES)?
    ))
}

/// True for strings [`new_run_id`] can produce.
pub fn is_run_id(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 15 + 2 * RUN_ID_RANDOM_BYTES
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'T'
        && b[9..13].iter().all(u8::is_ascii_digit)
        && b[13] == b'Z'
        && b[14] == b'-'
        && b[15..]
            .iter()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
}

/// What a run is, for `status` and the records: which runs count as a check of the tree.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum RunKind {
    /// `atelier check`.
    Check,
    /// `atelier check --compile-only`.
    CheckCompile,
    /// `atelier check --only <file>`: a red gate's failures re-run by name (bisection).
    CheckOnly,
    /// The gate of `merge`.
    Gate,
    /// `atelier test` of the whole suite.
    TestFull,
    /// `atelier test` of some tests only.
    TestNarrow,
    Follow,
    Tmp,
    /// A kind written by a later version, kept as written.
    Other(String),
}

impl RunKind {
    pub fn as_str(&self) -> &str {
        match self {
            RunKind::Check => "check",
            RunKind::CheckCompile => "check-compile",
            RunKind::CheckOnly => "check-only",
            RunKind::Gate => "gate",
            RunKind::TestFull => "test-full",
            RunKind::TestNarrow => "test-narrow",
            RunKind::Follow => "follow",
            RunKind::Tmp => "tmp",
            RunKind::Other(s) => s,
        }
    }
}

impl From<String> for RunKind {
    fn from(s: String) -> RunKind {
        match s.as_str() {
            "check" => RunKind::Check,
            "check-compile" => RunKind::CheckCompile,
            "check-only" => RunKind::CheckOnly,
            "gate" => RunKind::Gate,
            "test-full" => RunKind::TestFull,
            "test-narrow" => RunKind::TestNarrow,
            "follow" => RunKind::Follow,
            "tmp" => RunKind::Tmp,
            _ => RunKind::Other(s),
        }
    }
}

impl From<RunKind> for String {
    fn from(k: RunKind) -> String {
        k.as_str().to_owned()
    }
}

/// How a step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StepStatus {
    Passed,
    Failed,
    /// The leader ended but its group still had members, which were stopped.
    LeftProcesses,
    /// A stop signal reached atelier; the group was stopped.
    Interrupted,
    /// The program was never run: it could not be started, or atelier could not release it.
    /// A program that does not exist is run by the trampoline's shell, which fails (126 or 127).
    NotStarted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRecord {
    pub name: String,
    pub status: StepStatus,
    /// The leader's exit code.
    pub exit: Option<i32>,
    /// The signal that ended the leader.
    pub signal: Option<i32>,
    /// The log file, relative to the run folder.
    pub log: String,
    /// Processes left in the group when the leader ended (or when interrupted), `pid program`.
    pub left: Vec<String>,
    /// Processes that could not be stopped, even with KILL.
    pub unstoppable: Vec<String>,
}

impl StepRecord {
    pub fn passed(&self) -> bool {
        self.status == StepStatus::Passed
    }
}

/// The run's overall outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    Passed,
    Failed,
    Interrupted,
}

/// Written when a run starts: `<run folder>/run.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunInfo {
    pub id: String,
    pub kind: RunKind,
    pub command: String,
    /// The lock's machine key, [`LockKey::id`].
    pub lock: String,
    pub worktree: PathBuf,
    /// The branch the worktree was on, if any.
    pub branch: Option<String>,
    pub started: String,
    /// The atelier process that runs it.
    pub pid: u32,
    /// That process's [`procs::start_time`]: with it, a reused pid is not taken for the run.
    pub pid_start: Option<String>,
    pub atelier_version: String,
    /// `ATELIER_RUNS` of the run's steps: the outer runs' ids, then this run's.
    pub runs: Vec<String>,
}

impl RunInfo {
    pub fn read(run_dir: &Path) -> Result<Option<RunInfo>> {
        read_toml(&run_dir.join("run.toml"))
    }
}

/// Test counts of a run that ran tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TestCounts {
    pub passed: u64,
    pub failed: u64,
    /// Failed, then passed when retried.
    pub flaky: u64,
    pub skipped: u64,
    /// Not run because the settings exclude them.
    pub excluded: u64,
}

/// Written atomically when a run ends: `<run folder>/result`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunResult {
    pub id: String,
    pub kind: RunKind,
    pub command: String,
    pub outcome: Outcome,
    /// The tree id of what was tested.
    pub tree: Option<String>,
    /// `HEAD` when the tree was recorded.
    pub commit: Option<String>,
    /// The tree differs from `HEAD`'s.
    pub uncommitted: Option<bool>,
    pub branch: Option<String>,
    pub worktree: PathBuf,
    /// The platform the steps ran on, `<os>-<arch>`.
    pub platform: Option<String>,
    /// The machine from `machines.toml` the steps ran on; absent for this computer.
    pub machine: Option<String>,
    pub atelier_version: String,
    pub started: String,
    pub ended: String,
    /// The stop signal that ended the run.
    pub signal: Option<String>,
    pub failures: Vec<String>,
    /// Processes that left their group and still carried this run's marker at the end.
    pub survivors: Vec<String>,
    /// The per-test results (nextest's), relative to the run folder.
    pub test_results: Option<String>,
    pub tests: Option<TestCounts>,
    pub steps: Vec<StepRecord>,
}

impl RunResult {
    pub fn read(run_dir: &Path) -> Result<Option<RunResult>> {
        read_toml(&run_dir.join("result"))
    }
}

fn read_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read_to_string(path) {
        Ok(t) => toml::from_str(&t)
            .map(Some)
            .map_err(|e| Failure::refused(format!("{}: {}", path.display(), e.to_string().trim()))),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Failure::refused(format!("{}: {e}", path.display()))),
    }
}

/// The survivor scan a run uses: [`procs::marker_survivors`] unless a test replaces it.
pub type SurvivorScan = fn(&[String]) -> std::io::Result<Vec<(procs::ProcInfo, String)>>;

/// What a run's follow-up gave ([`Run::finish_after`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowUp {
    pub passed: bool,
    /// Lines for the run's failures.
    pub failures: Vec<String>,
}

/// A run in progress.
#[derive(Debug)]
pub struct Run {
    id: String,
    kind: RunKind,
    dir: PathBuf,
    command: String,
    lock: HeldLock,
    started: String,
    worktree: PathBuf,
    branch: Option<String>,
    platform: Option<String>,
    machine: Option<String>,
    /// `ATELIER_RUNS` of every step.
    runs: String,
    grace: Duration,
    steps: Vec<StepRecord>,
    failures: Vec<String>,
    tree: Option<String>,
    commit: Option<String>,
    uncommitted: Option<bool>,
    tests: Option<TestCounts>,
    test_results: Option<String>,
    interrupted: Option<i32>,
    /// A test's replacement for the end-of-run scan.
    scan: Option<SurvivorScan>,
    /// Step jobs started so far (macOS), for their labels and folders.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    jobs: u32,
    /// The folders of the runs this one is nested in, in the same state: a step's coalition is
    /// listed there too, so an outer run finds what a nested atelier left.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    outer_dirs: Vec<PathBuf>,
    _signals: signals::RunSignals,
}

impl Run {
    /// Takes `key` (refusing as [`HeldLock::acquire`] does), creates the run folder, writes
    /// `run.toml` and records the run under the lock. The stop signals are answered until the
    /// run is finished or dropped. The branch is the one `worktree`'s `HEAD` names, if any.
    pub fn start(
        state: &State,
        key: LockKey,
        kind: RunKind,
        command: &str,
        worktree: &Path,
    ) -> Result<Run> {
        Self::start_with_id(state, key, kind, command, worktree, None)
    }

    /// As [`Run::start`], with `preferred` as the run id when it is free (the id commands
    /// started before the run already carried as their marker).
    pub fn start_with_id(
        state: &State,
        key: LockKey,
        kind: RunKind,
        command: &str,
        worktree: &Path,
        preferred: Option<&str>,
    ) -> Result<Run> {
        let guard = signals::install()
            .map_err(|e| Failure::refused(format!("cannot install signal handlers: {e}")))?;
        let lock = HeldLock::acquire(state, key)?;
        Self::begin(state, lock, guard, kind, command, worktree, preferred)
    }

    /// As [`Run::start`], under a lock the caller already holds: for a command that reads and
    /// writes records under the lock before its run starts (`follow`).
    pub fn start_held(
        state: &State,
        lock: HeldLock,
        kind: RunKind,
        command: &str,
        worktree: &Path,
    ) -> Result<Run> {
        let guard = signals::install()
            .map_err(|e| Failure::refused(format!("cannot install signal handlers: {e}")))?;
        Self::begin(state, lock, guard, kind, command, worktree, None)
    }

    fn begin(
        state: &State,
        mut lock: HeldLock,
        guard: signals::RunSignals,
        kind: RunKind,
        command: &str,
        worktree: &Path,
        preferred: Option<&str>,
    ) -> Result<Run> {
        #[cfg(target_os = "macos")]
        crate::coalition::ensure_probed(state)?;
        let (id, dir) = create_run_dir(&state.runs_dir(), preferred)?;
        let started = fsutil::rfc3339(fsutil::now_secs());
        let mut chain = procs::inherited_runs();
        let outer_dirs = chain
            .iter()
            .map(|id| state.runs_dir().join(id))
            .filter(|d| is_run_id(&d.file_name().unwrap_or_default().to_string_lossy()))
            .filter(|d| d.is_dir())
            .collect();
        chain.push(id.clone());
        let branch = current_branch(worktree);
        let info = RunInfo {
            id: id.clone(),
            kind: kind.clone(),
            command: command.to_owned(),
            lock: lock.key().id(),
            worktree: worktree.to_path_buf(),
            branch: branch.clone(),
            started: started.clone(),
            pid: std::process::id(),
            pid_start: procs::start_time(std::process::id() as i32),
            atelier_version: VERSION.to_owned(),
            runs: chain.clone(),
        };
        let text = toml::to_string(&info).map_err(|e| Failure::refused(e.to_string()))?;
        fsutil::write_atomic(&dir.join("run.toml"), text.as_bytes()).ctx("run.toml")?;
        lock.add_run(&id)?;
        Ok(Run {
            id,
            kind,
            dir,
            command: command.to_owned(),
            lock,
            started,
            worktree: worktree.to_path_buf(),
            branch,
            platform: Platform::current().map(|p| p.to_string()),
            machine: None,
            runs: chain.join(":"),
            grace: procs::DEFAULT_GRACE,
            steps: Vec::new(),
            failures: Vec::new(),
            tree: None,
            commit: None,
            uncommitted: None,
            tests: None,
            test_results: None,
            interrupted: None,
            scan: None,
            jobs: 0,
            outer_dirs,
            _signals: guard,
        })
    }

    /// Sets the time a group gets between TERM and KILL.
    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    /// Replaces the survivor scan, for tests of a scan that fails.
    #[doc(hidden)]
    pub fn with_survivor_scan(mut self, scan: SurvivorScan) -> Self {
        self.scan = Some(scan);
        self
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn kind(&self) -> &RunKind {
        &self.kind
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn steps(&self) -> &[StepRecord] {
        &self.steps
    }

    /// The failure lines recorded so far.
    pub fn failures(&self) -> &[String] {
        &self.failures
    }

    /// The stop signal received during this run, if any.
    pub fn interrupted(&self) -> Option<i32> {
        self.interrupted.or_else(signals::received)
    }

    /// Records the tree the run tests.
    pub fn set_tree(&mut self, tree: String, commit: Option<String>, uncommitted: bool) {
        self.tree = Some(tree);
        self.commit = commit;
        self.uncommitted = Some(uncommitted);
    }

    /// Records the branch the run is about, when it is not the one its worktree is on (the
    /// gate's temporary worktree, for one).
    pub fn set_branch(&mut self, branch: Option<String>) {
        self.branch = branch;
    }

    /// Records the machine (its `machines.toml` name) and platform the steps ran on.
    pub fn set_machine(&mut self, machine: Option<String>, platform: Option<String>) {
        self.machine = machine;
        self.platform = platform;
    }

    /// Records the test counts.
    pub fn set_tests(&mut self, tests: TestCounts) {
        self.tests = Some(tests);
    }

    /// Records where the per-test results are, relative to the run folder.
    pub fn set_test_results(&mut self, rel: impl Into<String>) {
        self.test_results = Some(rel.into());
    }

    /// Adds a failure line to the result.
    pub fn fail(&mut self, line: impl Into<String>) {
        self.failures.push(line.into());
    }

    /// Runs one step to its end: its own process group, the run markers set, stdin closed,
    /// output in `<name>.log`. Returns its record (also kept for the result). After a stop
    /// signal no step starts. Refuses (2) an `env` that names a run marker, and a program whose
    /// name starts with `-` (the trampoline's `exec` would read it as an option).
    pub fn step(
        &mut self,
        name: &str,
        argv: &[String],
        cwd: &Path,
        env: &[(String, String)],
    ) -> Result<StepRecord> {
        self.step_with_stdout(name, argv, cwd, env, None)
    }

    /// As [`Run::step`], with the step's standard output in `stdout_file` (a name in the run
    /// folder) instead of the log; standard error stays in the log.
    pub fn step_with_stdout(
        &mut self,
        name: &str,
        argv: &[String],
        cwd: &Path,
        env: &[(String, String)],
        stdout_file: Option<&str>,
    ) -> Result<StepRecord> {
        let log_name = format!("{}.log", names::key(name));
        let mut record = StepRecord {
            name: name.to_owned(),
            status: StepStatus::NotStarted,
            exit: None,
            signal: None,
            log: log_name.clone(),
            left: Vec::new(),
            unstoppable: Vec::new(),
        };
        let Some(program) = argv.first() else {
            return Err(Failure::usage(format!("step {name}: empty command")));
        };
        if program.starts_with('-') {
            return Err(Failure::usage(format!(
                "step {name}: a program name cannot start with '-': {program:?}"
            )));
        }
        if let Some((k, _)) = env
            .iter()
            .find(|(k, _)| k == RUN_MARKER || k == RUNS_MARKER)
        {
            return Err(Failure::usage(format!(
                "step {name}: {k} is set by atelier and cannot be set by a step"
            )));
        }
        if let Some(sig) = self.interrupted() {
            self.interrupted = Some(sig);
            record.status = StepStatus::Interrupted;
            self.steps.push(record.clone());
            return Ok(record);
        }
        let log_path = self.dir.join(&log_name);
        #[cfg(target_os = "macos")]
        {
            let stdout_path = match stdout_file {
                Some(file) => self.dir.join(file),
                None => log_path.clone(),
            };
            self.job_step(record, argv, cwd, env, &stdout_path, &log_path)
        }
        #[cfg(not(target_os = "macos"))]
        self.group_step(record, argv, cwd, env, stdout_file, &log_path)
    }

    /// A step as its own process group, released by the `/bin/sh` trampoline (Linux).
    #[cfg(not(target_os = "macos"))]
    fn group_step(
        &mut self,
        mut record: StepRecord,
        argv: &[String],
        cwd: &Path,
        env: &[(String, String)],
        stdout_file: Option<&str>,
        log_path: &Path,
    ) -> Result<StepRecord> {
        let (program, args) = argv.split_first().expect("checked by the caller");
        let name = record.name.clone();
        let log_path = log_path.to_path_buf();
        let log = File::options()
            .create(true)
            .append(true)
            .open(&log_path)
            .ctx(log_path.display())?;
        let log_err = log.try_clone().ctx(log_path.display())?;
        let stdout: Stdio = match stdout_file {
            Some(file) => {
                let path = self.dir.join(file);
                File::options()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .ctx(path.display())?
                    .into()
            }
            None => log.into(),
        };
        // The step starts as a shell that waits on a pipe until its group is recorded under the
        // lock, then replaces itself with the program, stdin from /dev/null. If atelier dies
        // before releasing it, the pipe closes and the program never runs.
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", STEP_TRAMPOLINE, "atelier-step"])
            .arg(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(stdout)
            .stderr(log_err)
            .env_remove(DISPATCHED_VAR)
            .env_remove(RUN_ID_VAR)
            .process_group(0);
        for (k, v) in env {
            cmd.env(k, v);
        }
        // Last, so nothing above replaces them.
        cmd.env(RUN_MARKER, &self.id).env(RUNS_MARKER, &self.runs);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = fs::write(&log_path, format!("atelier: cannot start {program}: {e}\n"));
                self.steps.push(record.clone());
                return Ok(record);
            }
        };
        let pgid = child.id() as i32;
        let gate = child.stdin.take();
        // The shell is this process's unreaped child, so its pid is not reused meanwhile.
        let leader_start = procs::start_time(pgid);
        if let Err(e) = self.lock.add_group(pgid, &self.id, leader_start) {
            drop(gate);
            procs::stop_group(pgid, Some(&mut child), self.grace);
            return Err(e);
        }
        let released = match gate {
            Some(mut gate) => gate.write_all(b"go\n").is_ok(),
            None => false,
        };

        // The leader is never reaped here: until `stop_group` has emptied the group, it holds
        // the pgid.
        let ended = loop {
            match procs::exited(&child) {
                Ok(true) => break true,
                Ok(false) => {}
                Err(e) => {
                    procs::stop_group(pgid, Some(&mut child), self.grace);
                    return Err(Failure::refused(format!("waiting for step {name}: {e}")));
                }
            }
            if let Some(sig) = signals::received() {
                self.interrupted = Some(sig);
                break false;
            }
            sleep(Duration::from_millis(20));
        };

        let stopped = procs::stop_group(pgid, Some(&mut child), self.grace);
        let status = child.try_wait().ok().flatten();
        record.exit = status.and_then(|s| s.code());
        record.signal = status.and_then(|s| s.signal());
        record.left = stopped
            .members
            .iter()
            .map(procs::ProcInfo::describe)
            .collect();
        record.unstoppable = stopped
            .remaining
            .iter()
            .map(procs::ProcInfo::describe)
            .collect();
        record.status = if !ended {
            StepStatus::Interrupted
        } else if !released {
            let _ = writeln!(
                File::options()
                    .append(true)
                    .open(&log_path)
                    .ctx(log_path.display())?,
                "atelier: the step's shell ended before {program} could be released"
            );
            StepStatus::NotStarted
        } else if !stopped.was_empty() {
            // End-of-step group check: the leader is gone; the group must be empty.
            StepStatus::LeftProcesses
        } else if status.is_some_and(|s| s.success()) {
            StepStatus::Passed
        } else {
            StepStatus::Failed
        };
        if !procs::group_alive(pgid) {
            self.lock.remove_group(pgid)?;
        }
        self.steps.push(record.clone());
        Ok(record)
    }

    /// A step as a launchd job, released by atelier's shim (macOS, DESIGN §8.4).
    #[cfg(target_os = "macos")]
    fn job_step(
        &mut self,
        mut record: StepRecord,
        argv: &[String],
        cwd: &Path,
        env: &[(String, String)],
        stdout_path: &Path,
        log_path: &Path,
    ) -> Result<StepRecord> {
        use crate::coalition::{self, JobLine};
        use crate::launchd::{self, LaunchError};

        let program = &argv[0];
        let note = |line: &str| {
            if let Ok(mut f) = File::options().create(true).append(true).open(log_path) {
                let _ = writeln!(f, "atelier: {line}");
            }
        };
        // Created first, as the trampoline's redirections did: an empty log means the step ran
        // and printed nothing.
        File::options()
            .create(true)
            .append(true)
            .open(log_path)
            .ctx(log_path.display())?;
        self.jobs += 1;
        let label = format!("atelier.{}.{}", self.id, self.jobs);
        let job_dir = self.dir.join("jobs").join(self.jobs.to_string());
        fs::create_dir_all(&job_dir).ctx(job_dir.display())?;
        let spec = launchd::Spec {
            argv: argv.iter().map(Into::into).collect(),
            cwd: cwd.to_path_buf(),
            env: step_env(env, &self.id, &self.runs),
            stdout: stdout_path.to_path_buf(),
            stderr: log_path.to_path_buf(),
        };
        let mut job = match launchd::launch(&job_dir, &label, &spec, &mut || {
            signals::received().is_some()
        }) {
            Ok(j) => j,
            Err(LaunchError::Unavailable(m)) => {
                let _ = fs::remove_dir_all(&job_dir);
                return Err(Failure::refused(format!(
                    "step {}: cannot start it as a launchd job: {m}. atelier runs no step on macOS without one",
                    record.name
                )));
            }
            Err(LaunchError::NotStarted(m)) => {
                let _ = fs::remove_dir_all(&job_dir);
                note(&m);
                self.steps.push(record.clone());
                return Ok(record);
            }
            Err(LaunchError::Interrupted) => {
                let _ = fs::remove_dir_all(&job_dir);
                self.interrupted = signals::received();
                record.status = StepStatus::Interrupted;
                self.steps.push(record.clone());
                return Ok(record);
            }
        };
        let ids = [job.coalition.clone()];
        let line = JobLine {
            coalition: job.coalition.clone(),
            label: label.clone(),
        };
        // Listed before the program runs: this run's file, then every outer run's in this state.
        let listed = coalition::append_run_file(&self.dir, &line)
            .ctx(self.dir.join(coalition::RUN_FILE).display());
        for d in &self.outer_dirs {
            let _ = coalition::append_run_file(d, &line);
        }
        let recorded = listed.and_then(|()| {
            self.lock
                .add_job_group(job.pid, &self.id, job.start.clone(), &job.coalition)
        });
        if let Err(e) = recorded {
            job.abandon();
            let _ = job.wait(&mut || false);
            coalition::stop(&ids, self.grace);
            job.bootout();
            return Err(e);
        }
        let released = job.release();
        let ended = match job.wait(&mut || signals::received().is_some()) {
            Ok(e) => e,
            Err(e) => {
                coalition::stop(&ids, self.grace);
                job.bootout();
                return Err(Failure::refused(format!(
                    "waiting for step {}: {e}",
                    record.name
                )));
            }
        };
        if !ended {
            self.interrupted = signals::received();
        }
        // End-of-step coalition check: the job's process is gone; its coalition must be empty
        // once what ends on its own has ended.
        if ended {
            coalition::settle(&ids, coalition::SETTLE);
        }
        let stopped = coalition::stop(&ids, self.grace);
        let (exit, signal) = job.status();
        record.exit = exit;
        record.signal = signal;
        record.left = stopped
            .members
            .iter()
            .map(procs::ProcInfo::describe)
            .collect();
        record.unstoppable = stopped
            .remaining
            .iter()
            .map(procs::ProcInfo::describe)
            .collect();
        record.status = if !ended {
            StepStatus::Interrupted
        } else if !released {
            note(&format!(
                "the step's launchd job ended before {program} could be released"
            ));
            StepStatus::NotStarted
        } else if !stopped.was_empty() {
            StepStatus::LeftProcesses
        } else if exit == Some(0) {
            StepStatus::Passed
        } else {
            StepStatus::Failed
        };
        if stopped.remaining.is_empty() {
            self.lock.remove_group(job.pid)?;
        }
        if let Some(n) = job.bootout() {
            note(&n);
        }
        self.steps.push(record.clone());
        Ok(record)
    }

    /// Forgets the last step when it merely exited non-zero: a probe whose failure is an answer,
    /// not a failure of the run. Its log stays. A step that left processes behind, or did not
    /// start, is kept.
    pub fn discard_failed_step(&mut self) {
        if self
            .steps
            .last()
            .is_some_and(|s| s.status == StepStatus::Failed && s.unstoppable.is_empty())
        {
            self.steps.pop();
        }
    }

    /// Ends the run: the marker scan, then the result, written atomically. `passed` is the
    /// caller's verdict; failed steps, survivors, a failed scan and a stop signal each make the
    /// run fail. The run stays recorded under its lock while survivors live, and when the scan
    /// failed, so the next taker of the lock scans for it again.
    pub fn finish(self, passed: bool) -> Result<RunResult> {
        self.finish_keeping_lock(passed).map(|(r, _)| r)
    }

    /// As [`Run::finish`], handing the run's lock back to the caller instead of releasing it:
    /// for the gate, whose promotion follows its run under the same lock.
    pub fn finish_keeping_lock(self, passed: bool) -> Result<(RunResult, HeldLock)> {
        let (result, lock) = self.finish_then(
            passed,
            || unreachable!("not called without a follow-up"),
            false,
        )?;
        Ok((result, lock.expect("the lock is kept without a follow-up")))
    }

    /// As [`Run::finish`], then, when the run would pass, `then` runs before the result is
    /// written: this run's lock is released first (`then` is the platform's `atelier check`,
    /// which takes the same lock), and the result includes `then`'s verdict and lines. Until
    /// then the run has no result, so no reader takes it for a pass.
    pub fn finish_after(self, passed: bool, then: impl FnOnce() -> FollowUp) -> Result<RunResult> {
        self.finish_then(passed, then, true).map(|(r, _)| r)
    }

    /// The end-of-run scan: the survivors, described, and whether any still lives. On macOS,
    /// every live member of the coalitions in the run's coalitions file (its own steps' and
    /// those of runs nested in it), which are stopped; elsewhere the processes carrying the run's
    /// marker, which are not signalled.
    fn end_scan(&mut self) -> std::io::Result<(Vec<String>, bool)> {
        if let Some(scan) = self.scan {
            let found: Vec<String> = scan(std::slice::from_ref(&self.id))?
                .into_iter()
                .map(|(p, _)| p.describe())
                .collect();
            let alive = !found.is_empty();
            return Ok((found, alive));
        }
        #[cfg(target_os = "macos")]
        {
            let mut ids: Vec<crate::coalition::Coalition> = Vec::new();
            for l in crate::coalition::read_run_file(&self.dir)?.unwrap_or_default() {
                if !ids.contains(&l.coalition) {
                    ids.push(l.coalition);
                }
            }
            crate::coalition::settle(&ids, crate::coalition::SETTLE);
            let stopped = crate::coalition::stop(&ids, self.grace);
            if !stopped.remaining.is_empty() {
                self.failures.push(format!(
                    "could not stop: {}",
                    stopped
                        .remaining
                        .iter()
                        .map(procs::ProcInfo::describe)
                        .collect::<Vec<_>>()
                        .join("; ")
                ));
            }
            Ok((
                stopped
                    .members
                    .iter()
                    .map(procs::ProcInfo::describe)
                    .collect(),
                !stopped.remaining.is_empty(),
            ))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let found: Vec<String> = procs::marker_survivors(std::slice::from_ref(&self.id))?
                .into_iter()
                .map(|(p, _)| p.describe())
                .collect();
            let alive = !found.is_empty();
            Ok((found, alive))
        }
    }

    fn finish_then(
        mut self,
        passed: bool,
        then: impl FnOnce() -> FollowUp,
        follow: bool,
    ) -> Result<(RunResult, Option<HeldLock>)> {
        let (survivors, still_alive, scanned) = match self.end_scan() {
            Ok((found, alive)) => (found, alive, true),
            Err(e) => {
                self.failures
                    .push(format!("cannot scan for processes left behind: {e}"));
                (Vec::new(), true, false)
            }
        };
        for s in &self.steps {
            match s.status {
                StepStatus::Passed | StepStatus::Interrupted => {}
                StepStatus::Failed => self.failures.push(format!("{} failed", s.name)),
                StepStatus::NotStarted => self.failures.push(format!("{} could not start", s.name)),
                StepStatus::LeftProcesses => self.failures.push(format!(
                    "{} left processes behind: {}",
                    s.name,
                    s.left.join("; ")
                )),
            }
            if !s.unstoppable.is_empty() {
                self.failures.push(format!(
                    "{}: could not stop {}",
                    s.name,
                    s.unstoppable.join("; ")
                ));
            }
        }
        if !survivors.is_empty() {
            self.failures
                .push(format!("left processes behind: {}", survivors.join("; ")));
        }
        let mut interrupted = self.interrupted();
        let mut outcome = if interrupted.is_some() {
            Outcome::Interrupted
        } else if passed && self.failures.is_empty() {
            Outcome::Passed
        } else {
            Outcome::Failed
        };
        let mut lock = Some(self.lock);
        if follow && outcome == Outcome::Passed {
            // Passing means no survivor and a scan that worked: the run leaves the lock.
            if let Some(mut l) = lock.take() {
                l.remove_run(&self.id)?;
            }
            let f = then();
            self.failures.extend(f.failures);
            interrupted = self.interrupted.or_else(signals::received);
            outcome = if interrupted.is_some() {
                Outcome::Interrupted
            } else if f.passed && self.failures.is_empty() {
                Outcome::Passed
            } else {
                Outcome::Failed
            };
        }
        let result = RunResult {
            id: self.id.clone(),
            kind: self.kind.clone(),
            command: self.command.clone(),
            outcome,
            tree: self.tree.clone(),
            commit: self.commit.clone(),
            uncommitted: self.uncommitted,
            branch: self.branch.clone(),
            worktree: self.worktree.clone(),
            platform: self.platform.clone(),
            machine: self.machine.clone(),
            atelier_version: VERSION.to_owned(),
            started: self.started.clone(),
            ended: fsutil::rfc3339(fsutil::now_secs()),
            signal: interrupted.map(|s| signals::name(s).to_owned()),
            failures: self.failures.clone(),
            survivors: survivors.clone(),
            test_results: self.test_results.clone(),
            tests: self.tests,
            steps: self.steps.clone(),
        };
        let text = toml::to_string(&result).map_err(|e| Failure::refused(e.to_string()))?;
        fsutil::write_atomic(&self.dir.join("result"), text.as_bytes()).ctx("result")?;
        // Survivors keep the run recorded under the lock, so the next taker refuses while they
        // live; so does a scan that failed. Groups that could not be emptied stay recorded too.
        if let Some(l) = lock.as_mut()
            && scanned
            && !still_alive
        {
            l.remove_run(&self.id)?;
        }
        Ok((result, lock))
    }
}

/// A step's whole environment: atelier's own without the dispatcher's and the run-id variables,
/// then `env`, then the run markers, last so that nothing replaces them.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn step_env(
    env: &[(String, String)],
    id: &str,
    runs: &str,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    let mut out: Vec<(std::ffi::OsString, std::ffi::OsString)> = std::env::vars_os()
        .filter(|(k, _)| k != DISPATCHED_VAR && k != RUN_ID_VAR)
        .collect();
    let mut set = |k: &str, v: &str| {
        out.retain(|(x, _)| x != k);
        out.push((k.into(), v.into()));
    };
    for (k, v) in env {
        set(k, v);
    }
    set(RUN_MARKER, id);
    set(RUNS_MARKER, runs);
    out
}

/// The branch `HEAD` names in `worktree`, if it is a git worktree on a branch.
fn current_branch(worktree: &Path) -> Option<String> {
    let out = git::output(worktree, ["symbolic-ref", "-q", "HEAD"]).ok()?;
    if !out.status.success() {
        return None;
    }
    let full = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    full.strip_prefix("refs/heads/").map(str::to_owned)
}

fn create_run_dir(runs: &Path, preferred: Option<&str>) -> Result<(String, PathBuf)> {
    let mut preferred = preferred.filter(|id| is_run_id(id));
    for _ in 0..64 {
        let id = match preferred.take() {
            Some(id) => id.to_owned(),
            None => new_run_id()?,
        };
        let dir = runs.join(&id);
        match fs::create_dir(&dir) {
            Ok(()) => return Ok((id, dir)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(Failure::refused(format!("{}: {e}", dir.display()))),
        }
    }
    Err(Failure::refused("cannot create a new run folder"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ids_have_the_design_shape() {
        let id = new_run_id().unwrap();
        assert!(is_run_id(&id), "{id}");
        assert_eq!(id.len(), 31);
        assert!(is_run_id("20261113T1012Z-4f2a09c1d3e5b7a8"));
        assert!(!is_run_id("20261113T1012Z-4F2A09C1D3E5B7A8"));
        assert!(!is_run_id("20261113T1012Z-4f2a"));
        assert!(!is_run_id("../../etc/passwd00"));
        assert_ne!(new_run_id().unwrap(), id);
    }

    #[test]
    fn run_kinds_read_unknown_values_as_other() {
        for k in [
            RunKind::Check,
            RunKind::CheckCompile,
            RunKind::Gate,
            RunKind::TestFull,
            RunKind::TestNarrow,
            RunKind::Follow,
            RunKind::Tmp,
        ] {
            assert_eq!(RunKind::from(String::from(k.clone())), k);
        }
        assert_eq!(
            RunKind::from("bench".to_owned()),
            RunKind::Other("bench".into())
        );
    }
}
