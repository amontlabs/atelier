//! Runs (r4 §3.1): every run has its own id and folder `<state>/runs/<run id>/`, with its logs,
//! `run.toml` written when it starts and one `result` written atomically when it ends, naming the
//! exact tree it tested. A folder without `result` is a run that ended without a result.
//!
//! A run holds one lock for its whole life. Each step is started as the leader of its own
//! process group, with `ATELIER_RUN=<run id>` in its environment; its group is recorded under the
//! lock before atelier waits for it. When the leader ends, a group that still has members is
//! stopped (TERM, wait, KILL) and the step fails with "left processes behind". When the run ends,
//! the marker scan looks for processes that left the group; any found makes the run fail, is
//! named, and is not signalled. A TERM, INT or HUP stops the current group and the run ends with
//! a result that says so.

use std::fs::{self, File};
use std::io::ErrorKind;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::dispatch::DISPATCHED_VAR;
use crate::exit::{Failure, IoContext, Result};
use crate::fsutil;
use crate::locks::{HeldLock, LockKey};
use crate::names;
use crate::procs::{self, RUN_MARKER};
use crate::signals;
use crate::state::State;

/// A new run id: UTC time to the minute plus random bytes, `20261113T1012Z-4f2a`.
pub fn new_run_id() -> Result<String> {
    let (y, mo, d, h, mi, _) = fsutil::utc_parts(fsutil::now_secs());
    Ok(format!(
        "{y:04}{mo:02}{d:02}T{h:02}{mi:02}Z-{}",
        fsutil::random_hex(2)?
    ))
}

/// True for strings [`new_run_id`] can produce.
pub fn is_run_id(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 19
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'T'
        && b[9..13].iter().all(u8::is_ascii_digit)
        && b[13] == b'Z'
        && b[14] == b'-'
        && b[15..]
            .iter()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
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
    /// The program could not be started.
    NotStarted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// Written when a run starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunInfo {
    pub id: String,
    pub command: String,
    pub lock: String,
    pub worktree: PathBuf,
    pub started: String,
    pub pid: u32,
}

/// Written atomically when a run ends: `<run folder>/result`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunResult {
    pub id: String,
    pub command: String,
    pub outcome: Outcome,
    /// The tree id of what was tested.
    pub tree: Option<String>,
    /// `HEAD` when the tree was recorded.
    pub commit: Option<String>,
    /// The tree differs from `HEAD`'s.
    pub uncommitted: Option<bool>,
    pub started: String,
    pub ended: String,
    /// The stop signal that ended the run.
    pub signal: Option<String>,
    pub failures: Vec<String>,
    /// Processes that left their group and still carried this run's marker at the end.
    pub survivors: Vec<String>,
    pub steps: Vec<StepRecord>,
}

impl RunResult {
    pub fn read(run_dir: &Path) -> Result<Option<RunResult>> {
        let path = run_dir.join("result");
        match fs::read_to_string(&path) {
            Ok(t) => toml::from_str(&t).map(Some).map_err(|e| {
                Failure::refused(format!("{}: {}", path.display(), e.to_string().trim()))
            }),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Failure::refused(format!("{}: {e}", path.display()))),
        }
    }
}

/// A run in progress.
#[derive(Debug)]
pub struct Run {
    id: String,
    dir: PathBuf,
    command: String,
    lock: HeldLock,
    started: String,
    grace: Duration,
    steps: Vec<StepRecord>,
    failures: Vec<String>,
    tree: Option<String>,
    commit: Option<String>,
    uncommitted: Option<bool>,
    interrupted: Option<i32>,
}

impl Run {
    /// Takes `key` (refusing as [`HeldLock::acquire`] does), creates the run folder and records
    /// the run under the lock. Installs the stop-signal handlers.
    pub fn start(state: &State, key: LockKey, command: &str, worktree: &Path) -> Result<Run> {
        signals::install()
            .map_err(|e| Failure::refused(format!("cannot install signal handlers: {e}")))?;
        let mut lock = HeldLock::acquire(state, key)?;
        let (id, dir) = create_run_dir(&state.runs_dir())?;
        let started = fsutil::rfc3339(fsutil::now_secs());
        let info = RunInfo {
            id: id.clone(),
            command: command.to_owned(),
            lock: lock.key().describe(),
            worktree: worktree.to_path_buf(),
            started: started.clone(),
            pid: std::process::id(),
        };
        let text = toml::to_string(&info).map_err(|e| Failure::refused(e.to_string()))?;
        fsutil::write_atomic(&dir.join("run.toml"), text.as_bytes()).ctx("run.toml")?;
        lock.add_run(&id)?;
        Ok(Run {
            id,
            dir,
            command: command.to_owned(),
            lock,
            started,
            grace: procs::DEFAULT_GRACE,
            steps: Vec::new(),
            failures: Vec::new(),
            tree: None,
            commit: None,
            uncommitted: None,
            interrupted: None,
        })
    }

    /// Sets the time a group gets between TERM and KILL.
    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn steps(&self) -> &[StepRecord] {
        &self.steps
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

    /// Adds a failure line to the result.
    pub fn fail(&mut self, line: impl Into<String>) {
        self.failures.push(line.into());
    }

    /// Runs one step to its end: its own process group, `ATELIER_RUN` set, stdin closed, output
    /// in `<name>.log`. Returns its record (also kept for the result). After a stop signal no
    /// step starts.
    pub fn step(
        &mut self,
        name: &str,
        argv: &[String],
        cwd: &Path,
        env: &[(String, String)],
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
        if let Some(sig) = self.interrupted() {
            self.interrupted = Some(sig);
            record.status = StepStatus::Interrupted;
            self.steps.push(record.clone());
            return Ok(record);
        }
        let Some((program, args)) = argv.split_first() else {
            return Err(Failure::usage(format!("step {name}: empty command")));
        };
        let log_path = self.dir.join(&log_name);
        let log = File::options()
            .create(true)
            .append(true)
            .open(&log_path)
            .ctx(log_path.display())?;
        let log_err = log.try_clone().ctx(log_path.display())?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(log_err)
            .env_remove(DISPATCHED_VAR)
            .env(RUN_MARKER, &self.id)
            .process_group(0);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = fs::write(&log_path, format!("atelier: cannot start {program}: {e}\n"));
                self.steps.push(record.clone());
                return Ok(record);
            }
        };
        let pgid = child.id() as i32;
        if let Err(e) = self.lock.add_group(pgid, &self.id) {
            procs::stop_group(pgid, Some(&mut child), self.grace);
            return Err(e);
        }

        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(e) => return Err(Failure::refused(format!("waiting for step {name}: {e}"))),
            }
            if let Some(sig) = signals::received() {
                self.interrupted = Some(sig);
                break None;
            }
            sleep(Duration::from_millis(20));
        };

        match status {
            None => {
                let stopped = procs::stop_group(pgid, Some(&mut child), self.grace);
                let status = child.try_wait().ok().flatten();
                record.exit = status.and_then(|s| s.code());
                record.signal = status.and_then(|s| s.signal());
                record.status = StepStatus::Interrupted;
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
            }
            Some(status) => {
                record.exit = status.code();
                record.signal = status.signal();
                record.status = if status.success() {
                    StepStatus::Passed
                } else {
                    StepStatus::Failed
                };
                // End-of-step group check: the leader is gone; the group must be empty.
                let stopped = procs::stop_group(pgid, None, self.grace);
                if !stopped.was_empty() {
                    record.status = StepStatus::LeftProcesses;
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
                }
            }
        }
        if !procs::group_alive(pgid) {
            self.lock.remove_group(pgid)?;
        }
        self.steps.push(record.clone());
        Ok(record)
    }

    /// Ends the run: the marker scan, then the result, written atomically. `passed` is the
    /// caller's verdict; failed steps, survivors and a stop signal each make the run fail.
    pub fn finish(mut self, passed: bool) -> Result<RunResult> {
        let survivors = match procs::marker_survivors(std::slice::from_ref(&self.id)) {
            Ok(found) => found
                .into_iter()
                .map(|(p, _)| p.describe())
                .collect::<Vec<_>>(),
            Err(e) => {
                self.failures
                    .push(format!("cannot scan for processes left behind: {e}"));
                Vec::new()
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
        let interrupted = self.interrupted();
        let outcome = if interrupted.is_some() {
            Outcome::Interrupted
        } else if passed && self.failures.is_empty() {
            Outcome::Passed
        } else {
            Outcome::Failed
        };
        let result = RunResult {
            id: self.id.clone(),
            command: self.command.clone(),
            outcome,
            tree: self.tree.clone(),
            commit: self.commit.clone(),
            uncommitted: self.uncommitted,
            started: self.started.clone(),
            ended: fsutil::rfc3339(fsutil::now_secs()),
            signal: interrupted.map(|s| signals::name(s).to_owned()),
            failures: self.failures.clone(),
            survivors: survivors.clone(),
            steps: self.steps.clone(),
        };
        let text = toml::to_string(&result).map_err(|e| Failure::refused(e.to_string()))?;
        fsutil::write_atomic(&self.dir.join("result"), text.as_bytes()).ctx("result")?;
        // Survivors keep the run recorded under the lock, so the next taker refuses while they
        // live. Groups that could not be emptied stay recorded too.
        if survivors.is_empty() {
            self.lock.remove_run(&self.id)?;
        }
        Ok(result)
    }
}

fn create_run_dir(runs: &Path) -> Result<(String, PathBuf)> {
    for _ in 0..64 {
        let id = new_run_id()?;
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
        assert!(is_run_id("20261113T1012Z-4f2a"));
        assert!(!is_run_id("20261113T1012Z-4F2A"));
        assert!(!is_run_id("../../etc/passwd00"));
    }
}
