//! `atelier test <platform>|--all [-- <command>…]`, `atelier machines` and
//! `atelier machines init <name>` (DESIGN §2.5, §3).
//!
//! `test` runs this worktree's committed `HEAD` and refuses (4) uncommitted changes, which a
//! remote machine could not see, and a caller folder that is not in that commit; it has no
//! commit argument, because a run on this computer happens in place. It takes the tree id at
//! the start and checks it again before each platform, once a machine of this computer is
//! taken, and after the last platform: a change of the files or of `HEAD` is RED, "the tree
//! changed during the run". For each platform, one after the other, it takes this worktree's
//! run lock, chooses a machine ([`crate::machines::select`]) and runs the platform's job there:
//! on this computer in the worktree, under the machine lock; on another through `remote-run`,
//! whose results must record this commit and its tree. The final line says, per platform,
//! where it ran and which machines were skipped and why; it is printed even when a platform
//! ends in an error. Exit 0 pass, 1 fail, 3 no machine available (or the connection lost), 4
//! refused (the caller's tree: uncommitted changes, a folder not in the commit). A machine whose
//! checkout is in the way is not ready, and the next machine is tried.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::exit::{Exit, Failure, Result};
use crate::git;
use crate::jobs::{self, Ending, Job};
use crate::locks::LockKey;
use crate::machines::{
    self, Attempt, BUSY_POLL, Candidate, Machine, MachineLock, MachinesFile, Prober, Skip,
};
use crate::platform::{Os, Platform};
use crate::preflight::Context;
use crate::remote::{self, Installed, Probe, Remote, Session, Verdict};
use crate::runs::{Outcome, Run};
use crate::signals;
use crate::source;
use crate::tree;

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Failure::refused("HOME is not set"))
}

/// How one platform ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ended {
    Pass,
    Fail,
    NoMachine,
    ConnectionLost,
    Interrupted,
    /// The platform could not run; the error was printed, with its exit code.
    Error(Exit),
}

struct PlatformReport {
    platform: Platform,
    ended: Ended,
    /// The machine that ran it, and whether it is this computer.
    machine: Option<(String, bool)>,
    skipped: Vec<(String, Skip)>,
}

impl PlatformReport {
    fn line(&self) -> String {
        let skips = if self.skipped.is_empty() {
            String::new()
        } else {
            format!(" ({})", machines::skips_text(&self.skipped))
        };
        let on = match &self.machine {
            Some((name, true)) => format!(" on {name} (local)"),
            Some((name, false)) => format!(" on {name}"),
            None => String::new(),
        };
        let word = match self.ended {
            Ended::Pass => "PASS",
            Ended::Fail => "FAIL",
            Ended::Interrupted => "INTERRUPTED",
            Ended::ConnectionLost => "connection lost",
            Ended::NoMachine => "no machine available",
            Ended::Error(_) => "ERROR",
        };
        format!("{}: {word}{on}{skips}", self.platform)
    }
}

/// `atelier test`.
pub fn test(
    ctx: &Context,
    cwd: &Path,
    platform: Option<&str>,
    all: bool,
    command: Vec<String>,
) -> Result<Exit> {
    let mut platforms = Vec::new();
    if all {
        for name in ctx.settings.platforms.keys() {
            platforms.push(Platform::parse(name).expect("validated settings"));
        }
    } else if let Some(name) = platform {
        let p = Platform::parse(name).ok_or_else(|| {
            Failure::usage(format!(
                "{name:?} is not a platform: <os>-<arch>, os macos|linux|windows, arch aarch64|x86_64"
            ))
        })?;
        jobs::platform_settings(&ctx.settings, p)?;
        platforms.push(p);
    }
    for p in &platforms {
        machines::refuse_windows(*p)?;
    }

    let checkout = &ctx.checkout;
    let commit = checkout.head_commit()?.ok_or_else(|| {
        Failure::refused("this worktree has no commit yet: atelier test runs the committed HEAD")
    })?;
    let head_tree =
        tree::head_tree(checkout)?.ok_or_else(|| Failure::refused("HEAD has no tree"))?;
    if tree::tree_id(checkout)? != head_tree {
        return Err(Failure::refused(format!(
            "uncommitted changes: atelier test runs the committed HEAD, which another machine could see; commit first:\n  {}",
            status_lines(checkout).join("\n  ")
        )));
    }
    let rel = relative_folder(&checkout.root, cwd)?;
    in_commit(checkout, &commit, &rel)?;
    let home = home()?;
    let file = MachinesFile::load_owner(&home)?;
    let start = Start {
        commit: commit.clone(),
        tree: head_tree,
    };

    let mut reports = Vec::new();
    let mut changed = None;
    for p in platforms {
        if signals::received().is_some() {
            break;
        }
        if let Some(why) = start.changed(checkout) {
            changed = Some(why);
            break;
        }
        let job = Job {
            platform: p,
            command: (!command.is_empty()).then(|| command.clone()),
            rel: rel.clone(),
        };
        match run_platform(ctx, &file, &home, &start, &job) {
            Ok(r) => reports.push(r),
            Err(f) => {
                eprintln!("atelier: {p}: {}", f.message);
                reports.push(PlatformReport {
                    platform: p,
                    ended: Ended::Error(f.exit),
                    machine: None,
                    skipped: Vec::new(),
                });
            }
        }
    }
    if changed.is_none() {
        changed = start.changed(checkout);
    }
    let mut parts: Vec<String> = reports.iter().map(PlatformReport::line).collect();
    if let Some(why) = &changed {
        eprintln!("atelier: RED: the tree changed during the run: {why}");
        parts.push("RED: the tree changed during the run".to_owned());
    }
    println!("{}", parts.join(" · "));
    let ended = |e: &[Ended]| reports.iter().any(|r| e.contains(&r.ended));
    let error = reports.iter().find_map(|r| match r.ended {
        Ended::Error(e) => Some(e),
        _ => None,
    });
    Ok(
        if changed.is_some() || ended(&[Ended::Fail, Ended::Interrupted]) {
            Exit::Fail
        } else if let Some(e) = error {
            e
        } else if ended(&[Ended::NoMachine, Ended::ConnectionLost]) {
            Exit::NoMachine
        } else {
            Exit::Ok
        },
    )
}

/// The commit and tree `test` started with.
struct Start {
    commit: String,
    tree: String,
}

impl Start {
    /// Why the worktree is no longer the commit `test` started with: `HEAD` moved, or the files
    /// differ from it (uncommitted changes); `None` when it still is.
    fn changed(&self, checkout: &git::Checkout) -> Option<String> {
        match checkout.head_commit() {
            Ok(Some(h)) if h == self.commit => {}
            Ok(h) => {
                return Some(format!(
                    "HEAD moved from {} to {}",
                    self.commit,
                    h.as_deref().unwrap_or("nothing")
                ));
            }
            Err(f) => return Some(f.message),
        }
        match tree::tree_id(checkout) {
            Ok(t) if t == self.tree => None,
            Ok(t) => Some(format!(
                "tree {t}, not the commit's {}; uncommitted changes: {}",
                self.tree,
                status_lines(checkout).join("; ")
            )),
            Err(f) => Some(f.message),
        }
    }
}

/// Up to 20 lines of `git status --porcelain`, with untracked files.
fn status_lines(checkout: &git::Checkout) -> Vec<String> {
    git::run(
        &checkout.root,
        ["status", "--porcelain=v1", "--untracked-files=all"],
    )
    .unwrap_or_default()
    .lines()
    .take(20)
    .map(str::to_owned)
    .collect()
}

/// Refuses (4) a caller folder that is not a folder of `commit`: no other machine has it.
fn in_commit(checkout: &git::Checkout, commit: &str, rel: &Path) -> Result<()> {
    if rel.as_os_str().is_empty() {
        return Ok(());
    }
    let spec = format!("{commit}:{}", rel.to_string_lossy());
    let kind = git::output(&checkout.root, ["cat-file", "-t", &spec])?;
    if kind.status.success() && String::from_utf8_lossy(&kind.stdout).trim() == "tree" {
        Ok(())
    } else {
        Err(Failure::refused(format!(
            "{} is not a folder of the committed HEAD ({}): atelier test runs the commit, so run it from a folder the commit has",
            rel.display(),
            commit.get(..12).unwrap_or(commit)
        )))
    }
}

/// The caller's folder relative to the checkout root.
fn relative_folder(root: &Path, cwd: &Path) -> Result<PathBuf> {
    let root = git::canonical(root);
    let cwd = git::canonical(cwd);
    cwd.strip_prefix(&root).map(Path::to_path_buf).map_err(|_| {
        Failure::refused(format!(
            "{} is not inside {}",
            cwd.display(),
            root.display()
        ))
    })
}

/// What a machine that took the run hands over.
enum Taken {
    Local(MachineLock),
    Remote(Box<Session>),
}

struct RealProber<'a> {
    ctx: &'a Context,
    home: &'a Path,
    commit: &'a str,
    job: &'a Job,
    run_dir: PathBuf,
    told: bool,
}

impl Prober for RealProber<'_> {
    type Taken = Taken;

    fn attempt(&mut self, c: &Candidate) -> Attempt<Taken> {
        if c.machine.local {
            local_attempt(self.ctx, self.home, &c.machine, self.job)
        } else {
            let r = Remote::new(&c.name, &c.machine, &self.ctx.checkout);
            match remote::attempt(
                &r,
                &self.ctx.checkout,
                self.job,
                self.commit,
                &remote::log_path(&self.run_dir, &c.name),
            ) {
                Attempt::Taken(s) => Attempt::Taken(Taken::Remote(Box::new(s))),
                Attempt::Offline(d) => Attempt::Offline(d),
                Attempt::NotReady(l) => Attempt::NotReady(l),
                Attempt::Busy(h) => Attempt::Busy(h),
            }
        }
    }

    fn stopped(&self) -> bool {
        signals::received().is_some()
    }

    fn pause(&mut self, busy: &[&str]) -> bool {
        if !self.told {
            eprintln!(
                "{}: waiting for a busy machine to free up: {}",
                self.job.platform,
                busy.join(", ")
            );
            self.told = true;
        }
        let until = Instant::now() + BUSY_POLL;
        while Instant::now() < until {
            if signals::received().is_some() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        signals::received().is_none()
    }
}

fn local_attempt(ctx: &Context, home: &Path, m: &Machine, job: &Job) -> Attempt<Taken> {
    if let Some(first) = jobs::machine_problems(&ctx.checkout, &ctx.settings, m.platform())
        .into_iter()
        .next()
    {
        return Attempt::NotReady(first);
    }
    let holder = format!("{} ({})", ctx.checkout.root.display(), job.describe());
    match MachineLock::try_take(home, &holder) {
        Ok(Some(l)) => Attempt::Taken(Taken::Local(l)),
        Ok(None) => Attempt::Busy(MachineLock::holder(home)),
        Err(f) => Attempt::NotReady(f.message),
    }
}

fn run_platform(
    ctx: &Context,
    file: &MachinesFile,
    home: &Path,
    start: &Start,
    job: &Job,
) -> Result<PlatformReport> {
    let commit = start.commit.as_str();
    let ps = jobs::platform_settings(&ctx.settings, job.platform)?;
    let cands = machines::candidates(file, job.platform, ps);
    let mut run = Run::start(
        &ctx.state,
        LockKey::worktree(&ctx.checkout),
        job.kind(),
        &job.describe(),
        &ctx.checkout.root,
    )?;
    run.set_tree(start.tree.clone(), Some(commit.to_owned()), false);
    let mut prober = RealProber {
        ctx,
        home,
        commit,
        job,
        run_dir: run.dir().to_path_buf(),
        told: false,
    };
    let selection = machines::select(&cands, &mut prober);
    let skipped = selection.skipped;
    let run_dir = run.dir().to_path_buf();
    let id = run.id().to_owned();

    let Some((chosen, taken)) = selection.taken else {
        let details = if skipped.is_empty() {
            format!("no machine of {} in machines.toml", job.platform)
        } else {
            skipped
                .iter()
                .map(|(n, s)| format!("{n} {}", s.detail()))
                .collect::<Vec<_>>()
                .join("; ")
        };
        run.fail(format!("no machine available: {details}"));
        let result = run.finish(false)?;
        eprintln!("{}: no machine available:", job.platform);
        for (n, s) in &skipped {
            eprintln!("  {n}: {}", s.detail());
        }
        if skipped.is_empty() {
            eprintln!("  {details}");
        }
        let ended = if result.outcome == Outcome::Interrupted {
            Ended::Interrupted
        } else {
            Ended::NoMachine
        };
        return Ok(PlatformReport {
            platform: job.platform,
            ended,
            machine: None,
            skipped,
        });
    };

    let ended = match taken {
        Taken::Local(lock) => {
            run.set_machine(None, Some(job.platform.to_string()));
            // A wait for the machine may have let the files change.
            let ending = match start.changed(&ctx.checkout) {
                Some(why) => {
                    run.fail(format!("the tree changed during the run: {why}"));
                    Ok(Ending::Done(false))
                }
                None => jobs::run_steps(ctx, &mut run, job),
            };
            let ended = match ending {
                Ok(Ending::Done(passed)) => outcome_of(&run.finish(passed)?.outcome),
                Ok(Ending::ThenCheck) => outcome_of(
                    &run.finish_after(true, || jobs::full_check(ctx, &run_dir))?
                        .outcome,
                ),
                Err(f) => {
                    run.fail(f.message.clone());
                    let _ = run.finish(false);
                    return Err(f);
                }
            };
            drop(lock);
            ended
        }
        Taken::Remote(session) => {
            run.set_machine(Some(chosen.name.clone()), Some(job.platform.to_string()));
            let end = session.finish(commit, &start.tree);
            for f in &end.failures {
                run.fail(f.clone());
            }
            if let Some(Some(last)) = end.runs.last().map(|(_, r)| r.as_ref()) {
                if let Some(t) = last.tests {
                    run.set_tests(t);
                }
                // The tree the machine really tested.
                if let Some(tree) = &last.tree {
                    run.set_tree(
                        tree.clone(),
                        last.commit.clone(),
                        last.uncommitted.unwrap_or(true) || *tree != start.tree,
                    );
                }
            }
            let remote_ids: Vec<String> = end.runs.iter().map(|(i, _)| i.clone()).collect();
            let _ = std::fs::write(
                run_dir.join("remote-runs"),
                remote_ids
                    .iter()
                    .map(|i| format!("{i}\n"))
                    .collect::<String>(),
            );
            let passed = end.verdict == Verdict::Passed;
            let result = run.finish(passed)?;
            match end.verdict {
                Verdict::Passed if result.outcome == Outcome::Passed => Ended::Pass,
                Verdict::ConnectionLost => Ended::ConnectionLost,
                Verdict::Interrupted => Ended::Interrupted,
                _ if result.outcome == Outcome::Interrupted => Ended::Interrupted,
                _ => Ended::Fail,
            }
        }
    };
    let result = crate::runs::RunResult::read(&run_dir)?;
    eprintln!(
        "{}: run {id} on {}; folder {}",
        job.platform,
        chosen.name,
        run_dir.display()
    );
    if let Some(r) = result {
        for f in &r.failures {
            eprintln!("  {f}");
        }
    }
    Ok(PlatformReport {
        platform: job.platform,
        ended,
        machine: Some((chosen.name.clone(), chosen.machine.local)),
        skipped,
    })
}

fn outcome_of(o: &Outcome) -> Ended {
    match o {
        Outcome::Passed => Ended::Pass,
        Outcome::Failed => Ended::Fail,
        Outcome::Interrupted => Ended::Interrupted,
    }
}

/// `atelier machines`: every machine with its platform, GUI, priority, place, and whether it
/// is reachable, ready (with the first failing line; a remote machine's prerequisites only when
/// a run starts) and busy.
pub fn list(ctx: &Context) -> Result<Exit> {
    let home = home()?;
    let file = MachinesFile::load_owner(&home)?;
    let mut all: Vec<(&String, &Machine)> = file.machines.iter().collect();
    all.sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then(a.0.cmp(b.0)));
    let states: Vec<String> = std::thread::scope(|s| {
        let handles: Vec<_> = all
            .iter()
            .map(|(name, m)| {
                let home = &home;
                s.spawn(move || state_of(ctx, home, name, m))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| "?".to_owned()))
            .collect()
    });
    let width = all.iter().map(|(n, _)| n.len()).max().unwrap_or(4).max(4);
    println!(
        "{:width$}  {:14}  {:3}  {:>8}  {:20}  STATE",
        "NAME", "PLATFORM", "GUI", "PRIORITY", "WHERE"
    );
    for ((name, m), state) in all.iter().zip(states) {
        println!(
            "{:width$}  {:14}  {:3}  {:>8}  {:20}  {state}",
            name,
            m.platform,
            if m.gui { "yes" } else { "no" },
            m.priority,
            m.place(),
        );
    }
    Ok(Exit::Ok)
}

/// A remote machine's readiness as the probe knows it: the tool prerequisites depend on the
/// commit's files, so they are checked when a run starts there, and a machine that lacks one is
/// then passed over like one that is not ready.
const REMOTE_READY: &str = "ready (prerequisites checked at run time)";

fn state_of(ctx: &Context, home: &Path, name: &str, m: &Machine) -> String {
    if m.platform().os == Os::Windows {
        return "not supported (Windows)".to_owned();
    }
    if m.local {
        let ready = match jobs::machine_problems(&ctx.checkout, &ctx.settings, m.platform())
            .into_iter()
            .next()
        {
            None => "ready".to_owned(),
            Some(l) => format!("not ready: {l}"),
        };
        let busy = match MachineLock::held_by(home) {
            None => "free".to_owned(),
            Some(h) => format!("busy ({h})"),
        };
        return format!("this computer, {ready}, {busy}");
    }
    let r = Remote::new(name, m, &ctx.checkout);
    match r.probe(&m.platform) {
        Probe::Ready => format!("reachable, {REMOTE_READY}, free"),
        Probe::Busy(h) => format!("reachable, {REMOTE_READY}, busy ({h})"),
        Probe::NotReady(l) => format!("reachable, not ready: {l}"),
        Probe::Offline(d) => format!("offline ({d})"),
    }
}

/// `atelier machines init <name> [--yes]`: the project's checkout on that machine and the
/// pinned atelier there, built from this binary's own source. Nothing else is installed. It
/// prints its plan first; replacing an installed atelier of other source (which the machine's
/// own projects may use) needs `--yes`. The new build goes into a new folder that the machine's
/// atelier swaps in under the machine lock; the folder it replaces is kept.
pub fn init(ctx: &Context, name: &str, yes: bool) -> Result<Exit> {
    let home = home()?;
    let file = MachinesFile::load_owner(&home)?;
    let m = file.get(name)?;
    machines::refuse_windows(m.platform())?;
    if m.local {
        println!(
            "{name} is this computer: its checkout is this one and its atelier is the one running; nothing to set up"
        );
        return Ok(Exit::Ok);
    }
    let r = Remote::new(name, m, &ctx.checkout);
    match r.uname() {
        Ok(p) if p == m.platform => {}
        Ok(p) => {
            return Err(Failure::refused(format!(
                "{name}: uname says {p}; machines.toml declares {}",
                m.platform
            )));
        }
        Err(Probe::Offline(d)) => {
            return Err(Failure::new(
                Exit::NoMachine,
                format!("{name} offline: {d}"),
            ));
        }
        Err(Probe::NotReady(l)) | Err(Probe::Busy(l)) => {
            return Err(Failure::refused(format!("{name}: {l}")));
        }
        Err(Probe::Ready) => unreachable!(),
    }
    let has_checkout = r
        .has_checkout()
        .map_err(|e| Failure::refused(format!("{name}: {e}")))?;
    let want = source::identity();
    let have = r
        .installed_identity()
        .map_err(|e| Failure::refused(format!("{name}: {e}")))?;
    let place = format!("~/.local/share/atelier/{}/", crate::dispatch::VERSION);

    println!("{name}: plan");
    println!(
        "  checkout {}: {}",
        r.checkout,
        if has_checkout {
            "exists"
        } else {
            "create it (git init)"
        }
    );
    let what = |h: &str| {
        if h == crate::remote_run::BROKEN {
            "a broken install (no bin/atelier that answers)".to_owned()
        } else {
            format!("atelier {h}")
        }
    };
    match have.as_deref() {
        Some(h) if h == want => println!("  atelier {want}: installed already at {place}"),
        Some(h) => println!(
            "  atelier: replace {} at {place} with {want}, built there from this program's source (cargo install --locked), swapped in under the machine lock; the replaced folder is kept beside it, and an older replaced folder of this version is removed",
            what(h)
        ),
        None => println!(
            "  atelier {want}: build it there from this program's source (cargo install --locked) and install it at {place}"
        ),
    }
    if let Some(h) = have.as_deref()
        && h != want
        && !yes
    {
        return Err(Failure::refused(format!(
            "{name} has {} at {place}, which the machine's own projects may use: nothing was changed; run atelier machines init {name} --yes to replace it",
            what(h)
        )));
    }

    if !has_checkout {
        let made = r
            .create_checkout()
            .map_err(|e| Failure::refused(format!("{name}: cannot create the checkout: {e}")))?;
        println!("{name}: checkout {} ({made})", r.checkout);
    }
    if have.as_deref() == Some(want.as_str()) {
        return Ok(Exit::Ok);
    }
    eprintln!(
        "{name}: building atelier {} from this program's source there",
        crate::dispatch::VERSION
    );
    match r
        .install(have.as_deref())
        .map_err(|e| Failure::refused(format!("{name}: {e}")))?
    {
        Installed::Done(kept) => {
            if let Some(k) = kept {
                println!(
                    "{name}: the replaced install is kept at {k}; remove it when nothing runs from it"
                );
            }
        }
        Installed::Busy(h) => {
            return Err(Failure::new(
                Exit::NoMachine,
                format!(
                    "{name} is busy ({h}): the new build was not swapped in; run atelier machines init {name} again when it is free"
                ),
            ));
        }
        Installed::Changed(id) => {
            return Err(Failure::refused(format!(
                "{name}: the install there changed to {id} during the build: nothing was replaced; run atelier machines init {name} again"
            )));
        }
    }
    match r
        .installed_identity()
        .map_err(|e| Failure::refused(format!("{name}: {e}")))?
    {
        Some(id) if id == want => {
            println!("{name}: atelier {id} installed at {place}");
            Ok(Exit::Ok)
        }
        other => Err(Failure::refused(format!(
            "{name}: after the build, atelier there is {}, not {want}",
            other.as_deref().unwrap_or("missing")
        ))),
    }
}
