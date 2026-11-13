//! The machine's side of a remote run: `atelier remote-run`, an internal command that this
//! computer starts over ssh (see [`crate::remote`]). It is the same program as here, at the
//! same version, built from the same source, so locks, process groups, the tree comparison and
//! the preflight are the same code on both sides.
//!
//! `remote-run run`, in order: the platform from `uname`; the project's checkout and the pushed
//! commit; the machine preflight from that commit's settings; the machine lock (`busy`); a clean
//! checkout (changes and unknown files are listed, never deleted); the commit checked out; the
//! command preflight (version, settings, rules) and the checkout's run lock; then `taken`, and
//! the job's runs. A failure before `taken` is `not-ready <line>`.
//!
//! **The watcher.** Its input is the session's terminal, which this computer never writes to.
//! End-of-input or a read error means the connection is gone: the watcher sends this process a
//! hang-up, so the run stops its current group exactly as for a real `SIGHUP` (which the
//! terminal also delivers), and the result says `connection lost`.
//!
//! **The stream.** A thread copies the logs of this process's runs to the terminal, and
//! announces each run (`run <id>`) so that this computer can read its `result` afterwards.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::Duration;

use clap::Subcommand;

use crate::exit::{Exit, Result};
use crate::git;
use crate::jobs::{self, Ending, Job};
use crate::locks::{HeldLock, LockKey};
use crate::machines::MachineLock;
use crate::platform::Platform;
use crate::preflight::{self, Needs};
use crate::remote::tag;
use crate::runs::{Run, RunInfo};
use crate::settings::{SETTINGS_PATH, Settings};
use crate::signals;
use crate::source;

#[derive(Debug, Clone, Subcommand)]
pub enum RemoteAction {
    /// Print `<version> <source hash>`.
    Identity,
    /// Check the machine for a project without its commit: platform, checkout, preflight with
    /// the settings given on stdin, machine lock.
    Probe {
        #[command(flatten)]
        common: Common,
    },
    /// Run a platform job for a pushed commit.
    Run {
        #[command(flatten)]
        common: Common,
        #[arg(long)]
        commit: String,
        /// The caller's folder relative to the checkout root.
        #[arg(long)]
        dir: PathBuf,
        /// A narrow run's command; without it, the platform's whole run.
        #[arg(last = true)]
        command: Vec<String>,
    },
}

#[derive(Debug, Clone, clap::Args)]
pub struct Common {
    /// The machine's name in the owner's machines file.
    #[arg(long)]
    name: String,
    #[arg(long)]
    platform: String,
    /// The project's checkout on this machine (`~/…` or absolute).
    #[arg(long)]
    checkout: String,
    /// Marks this session's protocol lines.
    #[arg(long)]
    nonce: String,
}

/// Set by the watcher when the connection is gone.
static LOST: AtomicBool = AtomicBool::new(false);

pub fn main(action: RemoteAction) -> Result<Exit> {
    match action {
        RemoteAction::Identity => {
            say(&source::identity());
            Ok(Exit::Ok)
        }
        RemoteAction::Probe { common } => {
            let t = tag(&common.nonce);
            say(&format!("{t}identity {}", source::identity()));
            let mut text = String::new();
            let _ = std::io::stdin().read_to_string(&mut text);
            match probe(&common, &text) {
                Ok(None) => say(&format!("{t}ready")),
                Ok(Some(holder)) => say(&format!("{t}busy {holder}")),
                Err(line) => say(&format!("{t}not-ready {}", one_line(&line))),
            }
            Ok(Exit::Ok)
        }
        RemoteAction::Run {
            common,
            commit,
            dir,
            command,
        } => {
            start_watcher();
            let t = tag(&common.nonce);
            match run(&common, &commit, &dir, command) {
                Ok(()) => say(&format!("{t}done")),
                Err(Stop::NotReady(l)) => say(&format!("{t}not-ready {}", one_line(&l))),
                Err(Stop::Busy(h)) => say(&format!("{t}busy {h}")),
                Err(Stop::Error(e)) => say(&format!("{t}error {}", one_line(&e))),
            }
            Ok(Exit::Ok)
        }
    }
}

enum Stop {
    NotReady(String),
    Busy(String),
    /// After `taken`.
    Error(String),
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn expand(p: &str) -> PathBuf {
    if p == "~" {
        home()
    } else if let Some(rest) = p.strip_prefix("~/") {
        home().join(rest)
    } else {
        PathBuf::from(p)
    }
}

fn platform_of(common: &Common) -> std::result::Result<Platform, String> {
    let declared = Platform::parse(&common.platform)
        .ok_or_else(|| format!("{:?} is not a platform", common.platform))?;
    match jobs::uname_platform()? {
        p if p == declared.to_string() => Ok(declared),
        p => Err(format!(
            "uname says {p}; machines.toml declares {} {declared}",
            common.name
        )),
    }
}

fn checkout_of(common: &Common) -> std::result::Result<PathBuf, String> {
    let root = expand(&common.checkout);
    if root.join(".git").is_dir() {
        Ok(root)
    } else {
        Err(format!(
            "no checkout of this project at {}: run atelier machines init {}",
            common.checkout, common.name
        ))
    }
}

/// `Ok(None)` ready, `Ok(Some(holder))` busy, `Err(line)` not ready.
fn probe(common: &Common, settings_text: &str) -> std::result::Result<Option<String>, String> {
    let platform = platform_of(common)?;
    let root = checkout_of(common)?;
    if !settings_text.trim().is_empty() {
        let settings = Settings::parse(settings_text).map_err(|f| f.message)?;
        if let Some(first) = jobs::machine_problems(&settings, platform, &root)
            .into_iter()
            .next()
        {
            return Err(first);
        }
    }
    match MachineLock::try_take(&home(), "probe").map_err(|f| f.message)? {
        Some(_lock) => Ok(None),
        None => Ok(Some(MachineLock::holder(&home()))),
    }
}

fn git_ok(root: &Path, args: &[&str]) -> std::result::Result<String, String> {
    git::run(root, args).map_err(|f| f.message)
}

fn run(
    common: &Common,
    commit: &str,
    dir: &Path,
    command: Vec<String>,
) -> std::result::Result<(), Stop> {
    let t = tag(&common.nonce);
    let platform = platform_of(common).map_err(Stop::NotReady)?;
    let root = checkout_of(common).map_err(Stop::NotReady)?;
    if commit.len() < 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Stop::NotReady(format!(
            "{commit:?} is not a full commit id"
        )));
    }
    git_ok(&root, &["cat-file", "-e", &format!("{commit}^{{commit}}")]).map_err(|_| {
        Stop::NotReady(format!(
            "commit {commit} did not arrive in {}",
            common.checkout
        ))
    })?;
    let text = git_ok(&root, &["show", &format!("{commit}:{SETTINGS_PATH}")])
        .map_err(|e| Stop::NotReady(format!("{SETTINGS_PATH} of {commit}: {e}")))?;
    let settings = Settings::parse(&text).map_err(|f| Stop::NotReady(f.message))?;
    if let Some(first) = jobs::machine_problems(&settings, platform, &root)
        .into_iter()
        .next()
    {
        return Err(Stop::NotReady(first));
    }

    let holder = format!(
        "{} at {}",
        common.checkout,
        commit.get(..12).unwrap_or(commit)
    );
    let _machine = match MachineLock::try_take(&home(), &holder) {
        Ok(Some(l)) => l,
        Ok(None) => return Err(Stop::Busy(MachineLock::holder(&home()))),
        Err(f) => return Err(Stop::NotReady(f.message)),
    };

    let status = git_ok(
        &root,
        &["status", "--porcelain=v1", "--untracked-files=all"],
    )
    .map_err(Stop::NotReady)?;
    if !status.trim().is_empty() {
        let lines: Vec<&str> = status.lines().collect();
        let shown = lines
            .iter()
            .take(10)
            .copied()
            .collect::<Vec<_>>()
            .join("; ");
        let more = if lines.len() > 10 {
            format!(" and {} more", lines.len() - 10)
        } else {
            String::new()
        };
        return Err(Stop::NotReady(format!(
            "the checkout {} has changes; atelier deletes nothing there: {shown}{more}",
            common.checkout
        )));
    }
    git_ok(&root, &["checkout", "-q", "--detach", commit]).map_err(Stop::NotReady)?;

    let ctx = preflight::run(
        &root,
        "test",
        Needs {
            rules: true,
            versioned: false,
        },
    )
    .map_err(|f| Stop::NotReady(f.message))?;
    // Nothing else uses this checkout while the machine lock is held: taking its run lock
    // now finds an earlier run's processes before `taken`.
    drop(
        HeldLock::acquire(&ctx.state, LockKey::worktree(&ctx.checkout))
            .map_err(|f| Stop::NotReady(f.message))?,
    );
    let rel = dir.to_path_buf();
    if !root.join(&rel).is_dir() {
        return Err(Stop::NotReady(format!(
            "{} does not exist in commit {commit}",
            rel.display()
        )));
    }
    say(&format!("{t}taken"));

    let streamer = Streamer::start(ctx.state.runs_dir(), t.clone());
    let job = Job {
        platform,
        command: (!command.is_empty()).then_some(command),
        rel,
    };
    let outcome = run_job(&ctx, &job, &common.name);
    streamer.finish();
    outcome.map_err(Stop::Error)
}

fn run_job(ctx: &preflight::Context, job: &Job, name: &str) -> std::result::Result<(), String> {
    let mut run = Run::start(
        &ctx.state,
        LockKey::worktree(&ctx.checkout),
        job.kind(),
        &job.describe(),
        &ctx.checkout.root,
    )
    .map_err(|f| f.message)?;
    run.set_machine(Some(name.to_owned()), Some(job.platform.to_string()));
    let ending = jobs::run_steps(ctx, &mut run, job);
    if lost() {
        run.fail("connection lost");
    }
    match ending {
        Ok(Ending::Done(passed)) => {
            run.finish(passed).map_err(|f| f.message)?;
            Ok(())
        }
        Ok(Ending::ThenCheck) => {
            run.finish(true).map_err(|f| f.message)?;
            if lost() {
                return Ok(());
            }
            match jobs::full_check(ctx) {
                Ok(_) => Ok(()),
                Err(f) => Err(f.message),
            }
        }
        Err(f) => {
            run.fail(f.message.clone());
            let _ = run.finish(false);
            Err(f.message)
        }
    }
}

/// True once the connection is known to be gone.
fn lost() -> bool {
    LOST.load(Ordering::SeqCst) || signals::received() == Some(libc::SIGHUP)
}

/// Watches stdin: at end-of-input or a read error, the connection is gone.
fn start_watcher() {
    std::thread::spawn(|| {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 256];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        LOST.store(true, Ordering::SeqCst);
        // SAFETY: a signal to this process; during a run the handler records it, outside one
        // its default action ends the process, with nothing started.
        unsafe { libc::kill(libc::getpid(), libc::SIGHUP) };
    });
}

/// Prints one line to the terminal; a closed terminal is not an error.
fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn one_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Copies the logs of this process's runs to the terminal and announces each run.
struct Streamer {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

impl Streamer {
    fn start(runs_dir: PathBuf, tag: String) -> Streamer {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut s = StreamState {
                runs_dir,
                tag,
                mine: Vec::new(),
                others: BTreeSet::new(),
                offsets: BTreeMap::new(),
                last: None,
            };
            loop {
                let last_pass = flag.load(Ordering::SeqCst);
                s.pass(last_pass);
                if last_pass {
                    break;
                }
                sleep(Duration::from_millis(250));
            }
        });
        Streamer { stop, handle }
    }

    fn finish(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.handle.join();
    }
}

struct StreamState {
    runs_dir: PathBuf,
    tag: String,
    mine: Vec<PathBuf>,
    others: BTreeSet<PathBuf>,
    offsets: BTreeMap<PathBuf, u64>,
    last: Option<PathBuf>,
}

impl StreamState {
    fn pass(&mut self, last: bool) {
        let me = std::process::id();
        if let Ok(entries) = fs::read_dir(&self.runs_dir) {
            let mut new: Vec<(String, PathBuf)> = Vec::new();
            for e in entries.flatten() {
                let dir = e.path();
                if self.others.contains(&dir) || self.mine.contains(&dir) {
                    continue;
                }
                match RunInfo::read(&dir) {
                    Ok(Some(info)) if info.pid == me => {
                        new.push((info.started.clone() + &info.id, dir))
                    }
                    Ok(Some(_)) => {
                        self.others.insert(dir);
                    }
                    _ => {}
                }
            }
            new.sort();
            for (_, dir) in new {
                if let Some(id) = dir.file_name() {
                    say(&format!("{}run {}", self.tag, id.to_string_lossy()));
                }
                self.mine.push(dir);
            }
        }
        for dir in self.mine.clone() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            let mut logs: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "log"))
                .collect();
            logs.sort_by_key(|p| {
                fs::metadata(p)
                    .and_then(|m| m.created().or(m.modified()))
                    .ok()
            });
            for log in logs {
                self.copy(&log, last);
            }
        }
    }

    fn copy(&mut self, log: &Path, all: bool) {
        let offset = self.offsets.get(log).copied().unwrap_or(0);
        let Ok(mut f) = fs::File::open(log) else {
            return;
        };
        if f.seek(SeekFrom::Start(offset)).is_err() {
            return;
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() || buf.is_empty() {
            return;
        }
        // Whole lines only, so a protocol line is never glued to a log fragment; the last pass
        // ends an unfinished line itself.
        let mut chunk = match buf.iter().rposition(|b| *b == b'\n') {
            Some(end) => buf[..=end].to_vec(),
            None if all => Vec::new(),
            None => return,
        };
        let copied = if all { buf.len() } else { chunk.len() };
        if all && chunk.len() < buf.len() {
            chunk = buf.clone();
            chunk.push(b'\n');
        }
        let mut out = std::io::stdout().lock();
        if self.last.as_deref() != Some(log) {
            let name = log
                .parent()
                .and_then(Path::file_name)
                .map(|r| {
                    format!(
                        "{}/{}",
                        r.to_string_lossy(),
                        log.file_name().unwrap_or_default().to_string_lossy()
                    )
                })
                .unwrap_or_default();
            let _ = writeln!(out, "--- {name}");
            self.last = Some(log.to_path_buf());
        }
        let _ = out.write_all(&chunk);
        let _ = out.flush();
        self.offsets
            .insert(log.to_path_buf(), offset + copied as u64);
    }
}
