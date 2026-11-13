//! The machine's side of a remote run: `atelier remote-run`, an internal command that this
//! computer starts over ssh (see [`crate::remote`]). It is the same program as here, at the
//! same version, built from the same source, so locks, process groups, the tree comparison and
//! the preflight are the same code on both sides.
//!
//! `remote-run run`, in order: the platform from `uname`; the project's checkout and the pushed
//! commit; the machine lock (`busy`); a clean checkout (changes and unknown files are listed,
//! never deleted); the commit checked out by git, which refuses when a file it does not track,
//! ignored ones included, is in the way ([`check_out`]); the files exactly the commit's tree,
//! else the checkout goes back to where it was; the command preflight (version, settings,
//! rules); the machine preflight on the commit's files ([`jobs::machine_problems`]); the
//! checkout's run lock; then `taken`, and the job's runs. A failure before `taken` is
//! `not-ready <line>`: the caller moves on to its next machine, and the same machine gives the
//! same line until its owner moves the files it names.
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
use crate::runs::{FollowUp, Run, RunInfo};
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
            match install(&home(), &from, &expect) {
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
    check_out(&root, &common.checkout, commit, &commit_tree).map_err(Stop::NotReady)?;

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

/// Checks `commit` out in the clean checkout `root` with git's own `--no-overwrite-ignore`: git
/// refuses, before it changes anything, when a file it does not track (an ignored one included)
/// would be overwritten or removed, and names it. Then the files must be exactly the commit's
/// tree; when they are not (files ignored before are not ignored by this commit), the checkout
/// goes back to where it was. A refusal leaves the checkout as it was, so the same machine gives
/// the same answer every time. `Err` is the machine's not-ready line.
pub fn check_out(
    root: &Path,
    shown: &str,
    commit: &str,
    commit_tree: &str,
) -> std::result::Result<(), String> {
    let short = commit.get(..12).unwrap_or(commit);
    let old_head = git_ok(root, &["rev-parse", "-q", "--verify", "HEAD^{commit}"]).ok();
    let old_ref = git_ok(root, &["symbolic-ref", "-q", "HEAD"]).ok();
    let out = git::output(
        root,
        [
            "-c",
            "core.quotePath=false",
            "checkout",
            "-q",
            "--no-overwrite-ignore",
            "--detach",
            commit,
        ],
    )
    .map_err(|f| f.message)?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let named: Vec<&str> = err
            .lines()
            .filter_map(|l| l.strip_prefix('\t'))
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        return Err(if named.is_empty() {
            format!("git checkout {short} in {shown} failed: {}", one_line(&err))
        } else {
            format!(
                "checking out {short} in {shown} would overwrite or remove files that git does not track there (ignored ones included); atelier deletes nothing there, move them away: {}",
                listed(named)
            )
        });
    }

    let checkout = git::Checkout::find(root)
        .map_err(|f| f.message)?
        .ok_or_else(|| format!("{shown} is not a checkout"))?;
    let tree = tree::tree_id(&checkout).map_err(|f| f.message)?;
    if tree == commit_tree {
        return Ok(());
    }
    let status =
        git_ok(root, &["status", "--porcelain=v1", "--untracked-files=all"]).unwrap_or_default();
    let back = match go_back(root, commit, old_head.as_deref(), old_ref.as_deref()) {
        Ok(()) => format!(
            "the checkout is back at {}",
            old_head
                .as_deref()
                .and_then(|h| h.get(..12))
                .unwrap_or("no commit")
        ),
        Err(e) => format!("returning the checkout to where it was failed: {e}"),
    };
    Err(format!(
        "after checking out {short} the files in {shown} are not the commit's (tree {tree}, the commit's {commit_tree}): files git ignored before are not ignored by this commit; {back}; atelier deletes nothing there, move them away: {}",
        listed(status.lines())
    ))
}

/// Returns a checkout that has just checked `commit` out to `old_head` (none: no commit yet)
/// and to the branch `HEAD` named before, if any. Only files the two commits track change.
fn go_back(
    root: &Path,
    commit: &str,
    old_head: Option<&str>,
    old_ref: Option<&str>,
) -> std::result::Result<(), String> {
    match old_head {
        Some(h) => git_ok(
            root,
            &["checkout", "-q", "--no-overwrite-ignore", "--detach", h],
        )?,
        // No commit before: from the commit to the empty tree, and an empty index.
        None => git_ok(root, &["read-tree", "-m", "-u", commit, EMPTY_TREE])?,
    };
    if let Some(r) = old_ref {
        git_ok(root, &["symbolic-ref", "HEAD", r])?;
    }
    Ok(())
}

/// git's id of the empty tree.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// What `remote-run install` did.
#[derive(Debug, PartialEq)]
enum Installed {
    Done(Option<PathBuf>),
    Busy(String),
    Changed(String),
}

/// Swaps `from` in as this version's install folder under the machine lock, if the install
/// there is still `expect`. A failure before the swap removes `from` (this program's own new
/// folder, nothing else uses it).
fn install(home: &Path, from: &Path, expect: &str) -> std::result::Result<Installed, String> {
    let result = install_locked(home, from, expect);
    if !matches!(result, Ok(Installed::Done(_))) {
        let _ = fs::remove_dir_all(from);
    }
    result
}

fn install_locked(
    home: &Path,
    from: &Path,
    expect: &str,
) -> std::result::Result<Installed, String> {
    let _lock = match MachineLock::try_take(home, "atelier machines init") {
        Ok(Some(l)) => l,
        Ok(None) => return Ok(Installed::Busy(MachineLock::holder(home))),
        Err(f) => return Err(f.message),
    };
    let to = crate::dispatch::install_root(home).join(crate::dispatch::VERSION);
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
            let dir = run.dir().to_path_buf();
            run.finish_after(true, || {
                if lost() {
                    return FollowUp {
                        passed: false,
                        failures: vec!["connection lost".to_owned()],
                    };
                }
                jobs::full_check(ctx, &dir)
            })
            .map_err(|f| f.message)?;
            Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn g(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    /// Everything a checkout holds: `HEAD`, the branch it names, the index, and every path
    /// under the root (a nested repository's `.git` included) with its contents or target.
    fn snapshot(root: &Path) -> Vec<String> {
        let opt = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        };
        let mut all = vec![
            opt(&["rev-parse", "-q", "--verify", "HEAD"]),
            opt(&["symbolic-ref", "-q", "HEAD"]),
            opt(&["ls-files", "-s"]),
        ];
        fn walk(dir: &Path, rel: &str, top: bool, out: &mut Vec<String>) {
            let mut entries: Vec<_> = fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let name = e.file_name().to_string_lossy().into_owned();
                if top && name == ".git" {
                    continue;
                }
                let r = format!("{rel}{name}");
                let m = fs::symlink_metadata(e.path()).unwrap();
                if m.file_type().is_symlink() {
                    out.push(format!(
                        "L {r} {}",
                        fs::read_link(e.path()).unwrap().display()
                    ));
                } else if m.is_dir() {
                    out.push(format!("D {r}"));
                    walk(&e.path(), &format!("{r}/"), false, out);
                } else {
                    out.push(format!("F {r} {}", fs::read_to_string(e.path()).unwrap()));
                }
            }
        }
        walk(root, "", true, &mut all);
        all
    }

    /// The layouts of the 3.6 review (rounds 1 and 2): a machine checkout at an old commit,
    /// files there that git does not track, and a new commit that tracks paths in their way.
    /// Git refuses each one, before changing anything, naming the path; the same refusal comes
    /// back on a second try. Layouts that lose nothing are checked out.
    #[test]
    fn the_checkout_refuses_files_in_its_way_and_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().canonicalize().unwrap();
        // (layout, ignore rules of the old commit, what the new commit changes, what the
        // machine keeps, the path git names or None when the checkout goes ahead)
        type Setup = fn(&Path, &Path);
        let cases: Vec<(&str, &str, Setup, Setup, Option<&str>)> = vec![
            (
                "an ignored file the commit tracks",
                ".env\n",
                |s, _| write(s, ".env", "tracked\n"),
                |m, _| write(m, ".env", "machine secret\n"),
                Some(".env"),
            ),
            (
                "an ignored folder the commit makes a file",
                "local/\n",
                |s, _| write(s, "local", "a file\n"),
                |m, _| write(m, "local/keep.db", "machine data\n"),
                Some("local"),
            ),
            (
                "a folder inside an ignored folder the commit makes a file",
                "build/\n",
                |s, _| write(s, "build/cache", "a file\n"),
                |m, _| write(m, "build/cache/keep.db", "machine data\n"),
                Some("build/cache"),
            ),
            (
                "an ignored nested repository the commit makes a file",
                "build/\n",
                |s, _| write(s, "build/lib", "a file\n"),
                |m, _| {
                    fs::create_dir_all(m.join("build/lib")).unwrap();
                    g(&m.join("build/lib"), &["init", "-q"]);
                    write(m, "build/lib/work.txt", "machine work\n");
                },
                Some("build/lib"),
            ),
            (
                "an ignored link to a folder outside, under which the commit adds a file",
                "data\n",
                |s, _| write(s, "data/keep.db", "tracked\n"),
                |m, t| {
                    write(t, "outside/keep.db", "outside data\n");
                    std::os::unix::fs::symlink(t.join("outside"), m.join("data")).unwrap();
                },
                Some("data"),
            ),
            (
                "an ignored file in a tracked folder the commit makes a file",
                "olddir/ign2\n",
                |s, _| {
                    fs::remove_dir_all(s.join("olddir")).unwrap();
                    write(s, "olddir", "a file\n");
                },
                |m, _| write(m, "olddir/ign2", "kept beside tracked files\n"),
                Some("olddir"),
            ),
            (
                "a file added beside an ignored one",
                "later/\n",
                |s, _| write(s, "later/inside.txt", "tracked\n"),
                |m, _| write(m, "later/keep.txt", "untouched\n"),
                None,
            ),
        ];
        for (n, (layout, ignore, change, keep, named)) in cases.into_iter().enumerate() {
            let base = tmp.join(format!("case{n}"));
            let src = base.join("src");
            fs::create_dir_all(&src).unwrap();
            g(&src, &["init", "-q"]);
            write(&src, ".gitignore", ignore);
            write(&src, "olddir/f", "tracked\n");
            g(&src, &["add", "-A"]);
            g(&src, &["commit", "-q", "-m", "old"]);
            g(&base, &["clone", "-q", "src", "machine"]);
            let m = base.join("machine");
            g(&m, &["checkout", "-q", "--detach", "HEAD"]);
            change(&src, &base);
            g(&src, &["add", "-A", "-f"]);
            g(&src, &["commit", "-q", "-m", "new"]);
            let target = g(&src, &["rev-parse", "HEAD"]);
            let target_tree = g(&src, &["rev-parse", "HEAD^{tree}"]);
            g(&m, &["fetch", "-q", "origin"]);
            keep(&m, &base);
            assert_eq!(
                g(&m, &["status", "--porcelain", "--untracked-files=all"]),
                "",
                "{layout}: the machine's checkout is clean"
            );
            let before = snapshot(&m);
            let first = check_out(&m, "~/c", &target, &target_tree);
            match named {
                Some(path) => {
                    let e = first.clone().unwrap_err();
                    assert!(
                        e.contains("would overwrite or remove") && e.ends_with(path),
                        "{layout}: {e}"
                    );
                    assert_eq!(snapshot(&m), before, "{layout}: nothing changed");
                    let second = check_out(&m, "~/c", &target, &target_tree);
                    assert_eq!(second, first, "{layout}: the same answer again");
                    assert_eq!(snapshot(&m), before, "{layout}: nothing changed");
                }
                None => {
                    assert_eq!(first, Ok(()), "{layout}");
                    assert_eq!(g(&m, &["rev-parse", "HEAD"]), target);
                    assert_eq!(
                        fs::read_to_string(m.join("later/keep.txt")).unwrap(),
                        "untouched\n"
                    );
                }
            }
        }
        assert_eq!(
            fs::read_to_string(tmp.join("case4/outside/keep.db")).unwrap(),
            "outside data\n"
        );
    }

    /// On a volume that ignores case, `Build` and `build` are one path: a tracked file `Build`
    /// is in the way of an ignored folder `build`, and a rename of `Foo` to `foo` is a plain
    /// checkout. Where case matters, both go ahead.
    #[test]
    fn case_only_differences_follow_the_volume() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().canonicalize().unwrap();
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        g(&src, &["init", "-q"]);
        write(&src, ".gitignore", "build/\n");
        write(&src, "Foo", "x\n");
        g(&src, &["add", "-A"]);
        g(&src, &["commit", "-q", "-m", "old"]);
        g(&tmp, &["clone", "-q", "src", "machine"]);
        let m = tmp.join("machine");
        g(&m, &["checkout", "-q", "--detach", "HEAD"]);
        let ignores_case = g(&m, &["config", "--get", "core.ignorecase"]) == "true";

        // A case-only rename.
        g(&src, &["mv", "Foo", "foo.tmp"]);
        g(&src, &["mv", "foo.tmp", "foo"]);
        g(&src, &["commit", "-q", "-m", "rename"]);
        let renamed = g(&src, &["rev-parse", "HEAD"]);
        g(&m, &["fetch", "-q", "origin"]);
        let tree = g(&src, &["rev-parse", "HEAD^{tree}"]);
        assert_eq!(check_out(&m, "~/c", &renamed, &tree), Ok(()));
        let names: Vec<String> = fs::read_dir(&m)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != ".git" && n != ".gitignore")
            .collect();
        assert_eq!(names, ["foo"]);

        // A tracked `Build` where the machine keeps an ignored `build/`.
        write(&src, "Build", "tracked\n");
        g(&src, &["add", "-f", "Build"]);
        g(&src, &["commit", "-q", "-m", "Build"]);
        let target = g(&src, &["rev-parse", "HEAD"]);
        let tree = g(&src, &["rev-parse", "HEAD^{tree}"]);
        g(&m, &["fetch", "-q", "origin"]);
        write(&m, "build/keep.db", "machine data\n");
        let before = snapshot(&m);
        let r = check_out(&m, "~/c", &target, &tree);
        if ignores_case {
            let e = r.unwrap_err();
            assert!(e.contains("move them away: Build"), "{e}");
            assert_eq!(snapshot(&m), before);
        } else {
            assert_eq!(r, Ok(()));
            assert_eq!(
                fs::read_to_string(m.join("build/keep.db")).unwrap(),
                "machine data\n"
            );
        }
    }

    /// A commit that stops ignoring a file the machine keeps: the checkout goes ahead, the files
    /// are not the commit's tree, and the checkout goes back to where it was (also when it had
    /// no commit yet), so the next try gives the same answer.
    #[test]
    fn files_no_longer_ignored_send_the_checkout_back() {
        let tmp = tempfile::tempdir().unwrap();
        let tmp = tmp.path().canonicalize().unwrap();
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        g(&src, &["init", "-q"]);
        write(&src, ".gitignore", "stray.txt\n");
        write(&src, "a.txt", "a\n");
        g(&src, &["add", "-A"]);
        g(&src, &["commit", "-q", "-m", "old"]);
        g(&tmp, &["clone", "-q", "src", "machine"]);
        let m = tmp.join("machine");
        g(&m, &["checkout", "-q", "--detach", "HEAD"]);
        write(&src, ".gitignore", "!stray.txt\n");
        write(&src, "a.txt", "b\n");
        g(&src, &["commit", "-q", "-a", "-m", "new"]);
        let target = g(&src, &["rev-parse", "HEAD"]);
        let tree = g(&src, &["rev-parse", "HEAD^{tree}"]);
        g(&m, &["fetch", "-q", "origin"]);
        write(&m, "stray.txt", "the machine's\n");
        let before = snapshot(&m);
        let first = check_out(&m, "~/c", &target, &tree);
        let e = first.clone().unwrap_err();
        assert!(
            e.contains("are not the commit's")
                && e.contains("the checkout is back at")
                && e.ends_with("?? stray.txt"),
            "{e}"
        );
        assert_eq!(snapshot(&m), before);
        assert_eq!(check_out(&m, "~/c", &target, &tree), first);
        assert_eq!(snapshot(&m), before);

        // No commit yet: back to no commit, the branch `HEAD` named, and an empty index.
        let fresh = tmp.join("fresh");
        fs::create_dir_all(&fresh).unwrap();
        g(&fresh, &["init", "-q"]);
        write(&fresh, ".git/info/exclude", "stray.txt\n");
        write(&fresh, "stray.txt", "the machine's\n");
        g(&fresh, &["fetch", "-q", src.to_str().unwrap()]);
        let before = snapshot(&fresh);
        let e = check_out(&fresh, "~/c", &target, &tree).unwrap_err();
        assert!(e.contains("the checkout is back at no commit"), "{e}");
        assert_eq!(snapshot(&fresh), before);
    }

    fn fake_install(dir: &Path, identity: &str) {
        write(
            dir,
            "bin/atelier",
            &format!("#!/bin/sh\necho '{identity}'\n"),
        );
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir.join("bin/atelier"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn an_install_is_swapped_in_under_the_lock_and_the_old_one_kept() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let root = crate::dispatch::install_root(home);
        let to = root.join(crate::dispatch::VERSION);
        let new = |tag: &str| {
            let from = root.join(format!(".new-{tag}"));
            fake_install(&from, &format!("new {tag}"));
            from
        };
        // A lock this test just released can look held for a moment: a child that another test
        // thread is spawning holds a copy of the descriptor until it execs (see locks.rs). Such
        // a holder wrote no holder file.
        let install_after_release = |tag: &str, expect: &str| {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                let from = new(tag);
                match install(home, &from, expect) {
                    Ok(Installed::Busy(h))
                        if h.is_empty() && std::time::Instant::now() < deadline =>
                    {
                        sleep(Duration::from_millis(10));
                    }
                    other => return (from, other),
                }
            }
        };

        // Nothing installed yet: a plain rename.
        let (from, r) = install_after_release("a", "none");
        assert_eq!(r, Ok(Installed::Done(None)));
        assert!(!from.exists());
        assert!(
            fs::read_to_string(to.join("bin/atelier"))
                .unwrap()
                .contains("new a")
        );

        // Another install than expected: nothing replaced, the new folder removed.
        let (from, r) = install_after_release("b", "something else");
        assert_eq!(r, Ok(Installed::Changed("new a".to_owned())));
        assert!(!from.exists());
        assert!(
            fs::read_to_string(to.join("bin/atelier"))
                .unwrap()
                .contains("new a")
        );

        // The machine lock held: busy, nothing replaced.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let held = loop {
            match MachineLock::try_take(home, "a run").unwrap() {
                Some(l) => break l,
                None if std::time::Instant::now() < deadline => sleep(Duration::from_millis(10)),
                None => panic!("the machine lock stays held"),
            }
        };
        let from = new("c");
        match install(home, &from, "new a") {
            Ok(Installed::Busy(h)) => assert!(h.ends_with("a run"), "{h}"),
            other => panic!("{other:?}"),
        }
        assert!(!from.exists());
        drop(held);

        // The expected install: exchanged, the old folder kept beside it.
        let (from, r) = install_after_release("d", "new a");
        let kept = match r {
            Ok(Installed::Done(Some(k))) => k,
            other => panic!("{other:?}"),
        };
        assert!(
            fs::read_to_string(to.join("bin/atelier"))
                .unwrap()
                .contains("new d")
        );
        assert!(
            fs::read_to_string(kept.join("bin/atelier"))
                .unwrap()
                .contains("new a")
        );
        assert!(
            kept.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&format!(".{}.replaced-", crate::dispatch::VERSION))
        );
        assert!(!from.exists());
    }

    #[test]
    fn the_stream_takes_runs_by_id_or_by_pid_and_start_time_never_by_pid_alone() {
        let me = std::process::id();
        let start = procs::start_time(me as i32);
        assert!(start.is_some());
        let ids = Arc::new(Mutex::new(vec!["20260101T0000Z-aaaa".to_owned()]));
        let s = StreamState {
            runs_dir: PathBuf::new(),
            tag: String::new(),
            ids,
            me: (me, start.clone()),
            mine: Vec::new(),
            others: BTreeSet::new(),
            offsets: BTreeMap::new(),
            last: None,
        };
        let info = |id: &str, pid: u32, pid_start: Option<String>| RunInfo {
            id: id.to_owned(),
            kind: crate::runs::RunKind::Check,
            command: "check".to_owned(),
            lock: String::new(),
            worktree: PathBuf::new(),
            branch: None,
            started: String::new(),
            pid,
            pid_start,
            atelier_version: String::new(),
            runs: Vec::new(),
        };
        assert!(s.is_mine(&info("20260101T0000Z-aaaa", 1, None)));
        assert!(s.is_mine(&info("20260101T0000Z-bbbb", me, start.clone())));
        // An old run of an earlier process with the same pid.
        assert!(!s.is_mine(&info(
            "20200101T0000Z-cccc",
            me,
            Some("1.000000".to_owned())
        )));
        assert!(!s.is_mine(&info("20200101T0000Z-dddd", me, None)));
    }
}
