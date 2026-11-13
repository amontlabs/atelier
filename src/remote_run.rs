//! The machine's side of a remote run: `atelier remote-run`, an internal command that this
//! computer starts over ssh (see [`crate::remote`]). It is the same program as here, at the
//! same version, built from the same source, so locks, process groups, the tree comparison and
//! the preflight are the same code on both sides.
//!
//! `remote-run run`, in order: the platform from `uname`; the project's checkout and the pushed
//! commit; the machine lock (`busy`); a clean checkout (changes and unknown files are listed,
//! never deleted); no git-ignored file in the way of the checkout ([`checkout_obstructions`]);
//! the commit checked out; the files exactly the commit's tree; the command preflight (version,
//! settings, rules); the machine preflight on the commit's files ([`jobs::machine_problems`]);
//! the checkout's run lock; then `taken`, and the job's runs. A failure before `taken` is
//! `not-ready <line>`, or `refused <line>` for files in the checkout that atelier would have to
//! delete or that change what is tested (the owner must move them; the caller exits 4).
//!
//! `remote-run install` is run by `machines init` with the program it just built: under the
//! machine lock it swaps the new folder in as the pinned version's folder (one atomic exchange)
//! and keeps the folder it replaced beside it.
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
use crate::procs;
use crate::remote::tag;
use crate::runs::{Run, RunInfo};
use crate::signals;
use crate::source;
use crate::tree;

#[derive(Debug, Clone, Subcommand)]
pub enum RemoteAction {
    /// Print `<version> <source hash>`.
    Identity,
    /// Check the machine for a project without its commit: platform, checkout, and whether the
    /// machine lock is held.
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
        /// A narrow run's folder: the caller's, relative to the checkout root.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// A narrow run's command; without it, the platform's whole run.
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Swap a freshly built install folder in as this version's, under the machine lock.
    Install {
        /// The new folder (`<root>/bin/atelier` is this program).
        #[arg(long)]
        from: PathBuf,
        /// The identity the replaced install must have, or `none`.
        #[arg(long)]
        expect: String,
        #[arg(long)]
        nonce: String,
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
            match probe(&common) {
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
            match run(&common, &commit, dir.as_deref(), command) {
                Ok(()) => say(&format!("{t}done")),
                Err(Stop::Refused(l)) => say(&format!("{t}refused {}", one_line(&l))),
                Err(Stop::NotReady(l)) => say(&format!("{t}not-ready {}", one_line(&l))),
                Err(Stop::Busy(h)) => say(&format!("{t}busy {h}")),
                Err(Stop::Error(e)) => say(&format!("{t}error {}", one_line(&e))),
            }
            Ok(Exit::Ok)
        }
        RemoteAction::Install {
            from,
            expect,
            nonce,
        } => {
            let t = tag(&nonce);
            match install(&from, &expect) {
                Ok(Installed::Done(kept)) => say(&format!(
                    "{t}installed {}",
                    kept.map(|k| k.display().to_string()).unwrap_or_default()
                )),
                Ok(Installed::Busy(h)) => say(&format!("{t}busy {h}")),
                Ok(Installed::Changed(id)) => say(&format!("{t}changed {id}")),
                Err(e) => say(&format!("{t}error {}", one_line(&e))),
            }
            Ok(Exit::Ok)
        }
    }
}

enum Stop {
    /// Files in the checkout the owner must move: the caller exits 4.
    Refused(String),
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

/// `Ok(None)` ready, `Ok(Some(holder))` busy, `Err(line)` not ready. The lock is only looked
/// at ([`MachineLock::held_by`]), so a probe never makes a real run find the machine busy.
fn probe(common: &Common) -> std::result::Result<Option<String>, String> {
    platform_of(common)?;
    checkout_of(common)?;
    Ok(MachineLock::held_by(&home()))
}

fn git_ok(root: &Path, args: &[&str]) -> std::result::Result<String, String> {
    git::run(root, args).map_err(|f| f.message)
}

fn run(
    common: &Common,
    commit: &str,
    dir: Option<&Path>,
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
    let commit_tree =
        git_ok(&root, &["rev-parse", &format!("{commit}^{{tree}}")]).map_err(Stop::NotReady)?;

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
        return Err(Stop::NotReady(format!(
            "the checkout {} has changes; atelier deletes nothing there: {}",
            common.checkout,
            listed(status.lines())
        )));
    }
    let in_the_way = checkout_obstructions(&root, commit).map_err(Stop::NotReady)?;
    if !in_the_way.is_empty() {
        return Err(Stop::Refused(format!(
            "the checkout {} holds git-ignored files that checking out {} would overwrite or remove; atelier deletes nothing there, move them away: {}",
            common.checkout,
            commit.get(..12).unwrap_or(commit),
            listed(in_the_way.iter())
        )));
    }
    git_ok(&root, &["checkout", "-q", "--detach", commit]).map_err(Stop::NotReady)?;

    let checkout = git::Checkout::find(&root)
        .map_err(|f| Stop::NotReady(f.message))?
        .ok_or_else(|| Stop::NotReady(format!("{} is not a checkout", common.checkout)))?;
    let tree = tree::tree_id(&checkout).map_err(|f| Stop::NotReady(f.message))?;
    if tree != commit_tree {
        let status = git_ok(
            &root,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )
        .unwrap_or_default();
        return Err(Stop::Refused(format!(
            "after checking out {} the files in {} are not the commit's (tree {tree}, the commit's {commit_tree}): files git ignored before are not ignored by this commit; atelier deletes nothing there, move them away: {}",
            commit.get(..12).unwrap_or(commit),
            common.checkout,
            listed(status.lines())
        )));
    }

    let ctx = preflight::run(
        &root,
        "test",
        Needs {
            rules: true,
            versioned: false,
        },
    )
    .map_err(|f| Stop::NotReady(f.message))?;
    let problems = jobs::machine_problems(&ctx.checkout, &ctx.settings, platform);
    if let Some(first) = problems.first() {
        return Err(Stop::NotReady(match problems.len() {
            1 => first.clone(),
            n => format!("{first} (and {} more)", n - 1),
        }));
    }
    // Nothing else uses this checkout while the machine lock is held: taking its run lock
    // now finds an earlier run's processes before `taken`.
    drop(
        HeldLock::acquire(&ctx.state, LockKey::worktree(&ctx.checkout))
            .map_err(|f| Stop::NotReady(f.message))?,
    );
    let rel = dir.map(Path::to_path_buf).unwrap_or_default();
    if !command.is_empty() && !root.join(&rel).is_dir() {
        return Err(Stop::NotReady(format!(
            "{} does not exist in commit {commit}",
            rel.display()
        )));
    }
    say(&format!("{t}taken"));

    let mine = Arc::new(Mutex::new(Vec::new()));
    let streamer = Streamer::start(ctx.state.runs_dir(), t.clone(), Arc::clone(&mine));
    let job = Job {
        platform,
        command: (!command.is_empty()).then_some(command),
        rel,
    };
    let outcome = run_job(&ctx, &job, &common.name, &mine);
    streamer.finish();
    outcome.map_err(Stop::Error)
}

/// Up to ten items joined with `; `, then how many more.
fn listed<I: IntoIterator<Item = S>, S: AsRef<str>>(items: I) -> String {
    let items: Vec<String> = items.into_iter().map(|s| s.as_ref().to_owned()).collect();
    let shown = items
        .iter()
        .take(10)
        .cloned()
        .collect::<Vec<_>>()
        .join("; ");
    if items.len() > 10 {
        format!("{shown} and {} more", items.len() - 10)
    } else {
        shown
    }
}

/// The git-ignored files in `root` that `git checkout <target>` would overwrite or remove (git
/// treats ignored files as expendable). The checkout is clean, so anything on disk that `HEAD`
/// does not track is ignored. For every path `target` adds or changes against `HEAD` (every
/// path of `target` when there is no `HEAD`): an untracked file or link at that path, an
/// untracked file or link where one of its parent folders would go (DESIGN r4 §3.8 8A check e),
/// and the ignored files inside a folder at that path, which the checkout replaces by a file.
pub fn checkout_obstructions(
    root: &Path,
    target: &str,
) -> std::result::Result<Vec<String>, String> {
    let z = |args: &[&str]| -> std::result::Result<Vec<String>, String> {
        let out = git::output(root, args).map_err(|f| f.message)?;
        if !out.status.success() {
            return Err(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(out
            .stdout
            .split(|b| *b == 0)
            .filter(|p| !p.is_empty())
            .map(|p| String::from_utf8_lossy(p).into_owned())
            .collect())
    };
    let has_head = git::output(root, ["rev-parse", "-q", "--verify", "HEAD^{commit}"])
        .map_err(|f| f.message)?
        .status
        .success();
    let (tracked, changed): (BTreeSet<String>, Vec<String>) = if has_head {
        (
            z(&["ls-tree", "-r", "-z", "--name-only", "HEAD"])?
                .into_iter()
                .collect(),
            z(&[
                "diff",
                "--name-only",
                "-z",
                "--no-renames",
                "--diff-filter=AMT",
                "HEAD",
                target,
            ])?,
        )
    } else {
        (
            BTreeSet::new(),
            z(&["ls-tree", "-r", "-z", "--name-only", target])?,
        )
    };
    let ignored = z(&[
        "ls-files",
        "-z",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--directory",
    ])?;
    let mut found = BTreeSet::new();
    for p in &changed {
        let parts: Vec<&str> = p.split('/').collect();
        for i in 1..parts.len() {
            let q = parts[..i].join("/");
            match fs::symlink_metadata(root.join(&q)) {
                Ok(m) if m.is_dir() => {}
                Ok(_) if tracked.contains(&q) => {}
                Ok(_) => {
                    found.insert(q);
                }
                Err(_) => break,
            }
        }
        match fs::symlink_metadata(root.join(p)) {
            Err(_) => {}
            Ok(m) if m.is_dir() => {
                let inside = format!("{p}/");
                for e in &ignored {
                    if e.starts_with(&inside) {
                        found.insert(e.trim_end_matches('/').to_owned());
                    }
                }
            }
            Ok(_) if tracked.contains(p) => {}
            Ok(_) => {
                found.insert(p.clone());
            }
        }
    }
    Ok(found.into_iter().collect())
}

/// What `remote-run install` did.
enum Installed {
    Done(Option<PathBuf>),
    Busy(String),
    Changed(String),
}

/// Swaps `from` in as this version's install folder under the machine lock, if the install
/// there is still `expect`. A failure before the swap removes `from` (this program's own new
/// folder, nothing else uses it).
fn install(from: &Path, expect: &str) -> std::result::Result<Installed, String> {
    let result = install_locked(from, expect);
    if !matches!(result, Ok(Installed::Done(_))) {
        let _ = fs::remove_dir_all(from);
    }
    result
}

fn install_locked(from: &Path, expect: &str) -> std::result::Result<Installed, String> {
    let home = home();
    let _lock = match MachineLock::try_take(&home, "atelier machines init") {
        Ok(Some(l)) => l,
        Ok(None) => return Ok(Installed::Busy(MachineLock::holder(&home))),
        Err(f) => return Err(f.message),
    };
    let to = crate::dispatch::install_root(&home).join(crate::dispatch::VERSION);
    let bin = to.join("bin/atelier");
    let found = if bin.exists() {
        let out = std::process::Command::new(&bin)
            .args(["remote-run", "identity"])
            .env(crate::dispatch::DISPATCHED_VAR, "1")
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("{}: {e}", bin.display()))?;
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    } else if fs::symlink_metadata(&to).is_ok() {
        "an install without bin/atelier".to_owned()
    } else {
        "none".to_owned()
    };
    if found != expect {
        return Ok(Installed::Changed(found));
    }
    swap_in(from, &to).map(Installed::Done)
}

/// Puts `from` at `to` in one step: a rename when `to` is absent, else an atomic exchange of the
/// two (no moment without an install), after which the old folder is renamed to
/// `.<name>.replaced-<time>-<pid>` beside it and kept: a program may still run from it.
pub fn swap_in(from: &Path, to: &Path) -> std::result::Result<Option<PathBuf>, String> {
    let shown = |e: std::io::Error| format!("{} → {}: {e}", from.display(), to.display());
    if fs::symlink_metadata(to).is_err() {
        fs::rename(from, to).map_err(shown)?;
        return Ok(None);
    }
    exchange(from, to).map_err(shown)?;
    let name = to
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (y, mo, d, h, mi, sec) = crate::fsutil::utc_parts(crate::fsutil::now_secs());
    let kept = to.with_file_name(format!(
        ".{name}.replaced-{y:04}{mo:02}{d:02}T{h:02}{mi:02}{sec:02}Z-{}",
        std::process::id()
    ));
    fs::rename(from, &kept).map_err(|e| {
        format!(
            "the new install is in place; the old one is at {} ({e})",
            from.display()
        )
    })?;
    Ok(Some(kept))
}

/// Exchanges two paths atomically.
fn exchange(a: &Path, b: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let ca = CString::new(a.as_os_str().as_bytes())?;
    let cb = CString::new(b.as_os_str().as_bytes())?;
    #[cfg(target_os = "macos")]
    // SAFETY: two NUL-terminated paths.
    let r = unsafe { libc::renamex_np(ca.as_ptr(), cb.as_ptr(), libc::RENAME_SWAP) };
    #[cfg(target_os = "linux")]
    // SAFETY: two NUL-terminated paths relative to the current folder.
    let r = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            ca.as_ptr(),
            libc::AT_FDCWD,
            cb.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn run_job(
    ctx: &preflight::Context,
    job: &Job,
    name: &str,
    mine: &Mutex<Vec<String>>,
) -> std::result::Result<(), String> {
    let mut run = Run::start(
        &ctx.state,
        LockKey::worktree(&ctx.checkout),
        job.kind(),
        &job.describe(),
        &ctx.checkout.root,
    )
    .map_err(|f| f.message)?;
    mine.lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(run.id().to_owned());
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

/// Copies the logs of this process's runs to the terminal and announces each run: the runs
/// whose ids `run_job` hands over, and the runs `atelier check` starts in this process, found by
/// pid and the process's start time (a run folder left by an earlier process with the same pid
/// has another start time).
struct Streamer {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

impl Streamer {
    fn start(runs_dir: PathBuf, tag: String, ids: Arc<Mutex<Vec<String>>>) -> Streamer {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut s = StreamState {
                runs_dir,
                tag,
                ids,
                me: (
                    std::process::id(),
                    procs::start_time(std::process::id() as i32),
                ),
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
    /// Run ids this process started and announced itself.
    ids: Arc<Mutex<Vec<String>>>,
    /// This process's pid and start time.
    me: (u32, Option<String>),
    mine: Vec<PathBuf>,
    others: BTreeSet<PathBuf>,
    offsets: BTreeMap<PathBuf, u64>,
    last: Option<PathBuf>,
}

impl StreamState {
    /// True when the run in `dir` is this process's.
    fn is_mine(&self, info: &RunInfo) -> bool {
        let known = self
            .ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&info.id);
        known || (info.pid == self.me.0 && self.me.1.is_some() && info.pid_start == self.me.1)
    }

    fn pass(&mut self, last: bool) {
        if let Ok(entries) = fs::read_dir(&self.runs_dir) {
            let mut new: Vec<(String, PathBuf)> = Vec::new();
            for e in entries.flatten() {
                let dir = e.path();
                if self.others.contains(&dir) || self.mine.contains(&dir) {
                    continue;
                }
                match RunInfo::read(&dir) {
                    Ok(Some(info)) if self.is_mine(&info) => {
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
