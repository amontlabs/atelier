//! `atelier test <platform>|--all [-- <command>…]`, `atelier machines` and
//! `atelier machines init <name>` (DESIGN §2.5, §3).
//!
//! `test` runs this worktree's committed `HEAD` and refuses (4) uncommitted changes, which a
//! remote machine could not see; it has no commit argument, because a run on this computer
//! happens in place. For each platform, one after the other, it takes this worktree's run lock,
//! chooses a machine ([`crate::machines::select`]) and runs the platform's job there: on this
//! computer in the worktree, under the machine lock; on another through `remote-run`. The final
//! line says, per platform, where it ran and which machines were skipped and why. Exit 0 pass,
//! 1 fail, 3 no machine available (or the connection lost), 4 refused.

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
use crate::remote::{self, Probe, Remote, Session, Verdict};
use crate::runs::{Outcome, Run};
use crate::settings::SETTINGS_PATH;
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
    let tree = tree::tree_id(checkout)?;
    if tree::head_tree(checkout)?.as_deref() != Some(tree.as_str()) {
        let status = git::run(
            &checkout.root,
            ["status", "--porcelain=v1", "--untracked-files=all"],
        )
        .unwrap_or_default();
        return Err(Failure::refused(format!(
            "uncommitted changes: atelier test runs the committed HEAD, which another machine could see; commit first:\n  {}",
            status.lines().take(20).collect::<Vec<_>>().join("\n  ")
        )));
    }
    let rel = relative_folder(&checkout.root, cwd)?;
    let home = home()?;
    let file = MachinesFile::load_owner(&home)?;
    let settings_text = std::fs::read_to_string(checkout.root.join(SETTINGS_PATH))
        .map_err(|e| Failure::refused(format!("{SETTINGS_PATH}: {e}")))?;

    let mut reports = Vec::new();
    for p in platforms {
        if signals::received().is_some() {
            break;
        }
        let job = Job {
            platform: p,
            command: (!command.is_empty()).then(|| command.clone()),
            rel: rel.clone(),
        };
        reports.push(run_platform(
            ctx,
            &file,
            &home,
            &settings_text,
            &commit,
            &job,
        )?);
    }
    let line = reports
        .iter()
        .map(PlatformReport::line)
        .collect::<Vec<_>>()
        .join(" · ");
    println!("{line}");
    let ended = |e: &[Ended]| reports.iter().any(|r| e.contains(&r.ended));
    Ok(if ended(&[Ended::Fail, Ended::Interrupted]) {
        Exit::Fail
    } else if ended(&[Ended::NoMachine, Ended::ConnectionLost]) {
        Exit::NoMachine
    } else {
        Exit::Ok
    })
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
    settings_text: &'a str,
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
                self.settings_text,
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
    if let Some(first) = jobs::machine_problems(&ctx.settings, m.platform(), &ctx.checkout.root)
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
    settings_text: &str,
    commit: &str,
    job: &Job,
) -> Result<PlatformReport> {
    let ps = jobs::platform_settings(&ctx.settings, job.platform)?;
    let cands = machines::candidates(file, job.platform, ps);
    let mut run = Run::start(
        &ctx.state,
        LockKey::worktree(&ctx.checkout),
        job.kind(),
        &job.describe(),
        &ctx.checkout.root,
    )?;
    let tree = tree::tree_id(&ctx.checkout)?;
    run.set_tree(tree, Some(commit.to_owned()), false);
    let mut prober = RealProber {
        ctx,
        home,
        settings_text,
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
            let ending = jobs::run_steps(ctx, &mut run, job);
            let ended = match ending {
                Ok(Ending::Done(passed)) => outcome_of(&run.finish(passed)?.outcome),
                Ok(Ending::ThenCheck) => {
                    let prep = run.finish(true)?;
                    if prep.outcome != Outcome::Passed {
                        outcome_of(&prep.outcome)
                    } else {
                        match jobs::full_check(ctx) {
                            Ok(Exit::Ok) => Ended::Pass,
                            Ok(_) => Ended::Fail,
                            Err(f) => {
                                eprintln!("atelier: {}", f.message);
                                Ended::Fail
                            }
                        }
                    }
                }
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
            let end = session.finish();
            for f in &end.failures {
                run.fail(f.clone());
            }
            if let Some(Some(last)) = end.runs.last().map(|(_, r)| r.as_ref())
                && let Some(t) = last.tests
            {
                run.set_tests(t);
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
/// is reachable, ready (with the first failing line) and busy.
pub fn list(ctx: &Context) -> Result<Exit> {
    let home = home()?;
    let file = MachinesFile::load_owner(&home)?;
    let settings_text = std::fs::read_to_string(ctx.checkout.root.join(SETTINGS_PATH))
        .map_err(|e| Failure::refused(format!("{SETTINGS_PATH}: {e}")))?;
    let mut all: Vec<(&String, &Machine)> = file.machines.iter().collect();
    all.sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then(a.0.cmp(b.0)));
    let states: Vec<String> = std::thread::scope(|s| {
        let handles: Vec<_> = all
            .iter()
            .map(|(name, m)| {
                let (home, text) = (&home, &settings_text);
                s.spawn(move || state_of(ctx, home, name, m, text))
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

fn state_of(ctx: &Context, home: &Path, name: &str, m: &Machine, settings_text: &str) -> String {
    if m.platform().os == Os::Windows {
        return "not supported (Windows)".to_owned();
    }
    if m.local {
        let ready = match jobs::machine_problems(&ctx.settings, m.platform(), &ctx.checkout.root)
            .into_iter()
            .next()
        {
            None => "ready".to_owned(),
            Some(l) => format!("not ready: {l}"),
        };
        let busy = match MachineLock::try_take(home, "atelier machines") {
            Ok(Some(_)) => "free".to_owned(),
            Ok(None) => format!("busy ({})", MachineLock::holder(home)),
            Err(f) => format!("lock: {}", f.message),
        };
        return format!("this computer, {ready}, {busy}");
    }
    let r = Remote::new(name, m, &ctx.checkout);
    match r.probe(&m.platform, settings_text) {
        Probe::Ready => "reachable, ready, free".to_owned(),
        Probe::Busy(h) => format!("reachable, ready, busy ({h})"),
        Probe::NotReady(l) => format!("reachable, not ready: {l}"),
        Probe::Offline(d) => format!("offline ({d})"),
    }
}

/// `atelier machines init <name>`: the project's checkout on that machine and the pinned
/// atelier there, built from this binary's own source. Nothing else is installed.
pub fn init(ctx: &Context, name: &str) -> Result<Exit> {
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
    let made = r
        .create_checkout()
        .map_err(|e| Failure::refused(format!("{name}: cannot create the checkout: {e}")))?;
    println!("{name}: checkout {} ({made})", r.checkout);
    let want = source::identity();
    let have = r
        .installed_identity()
        .map_err(|e| Failure::refused(format!("{name}: {e}")))?;
    if have.as_deref() == Some(want.as_str()) {
        println!("{name}: atelier {want} already installed");
        return Ok(Exit::Ok);
    }
    eprintln!(
        "{name}: building atelier {} from this program's source there (cargo install --locked){}",
        crate::dispatch::VERSION,
        have.map(|h| format!("; replaces {h}")).unwrap_or_default()
    );
    r.install()
        .map_err(|e| Failure::refused(format!("{name}: {e}")))?;
    match r
        .installed_identity()
        .map_err(|e| Failure::refused(format!("{name}: {e}")))?
    {
        Some(id) if id == want => {
            println!(
                "{name}: atelier {id} installed at ~/.local/share/atelier/{}/",
                crate::dispatch::VERSION
            );
            Ok(Exit::Ok)
        }
        other => Err(Failure::refused(format!(
            "{name}: after the build, atelier there is {}, not {want}",
            other.as_deref().unwrap_or("missing")
        ))),
    }
}
