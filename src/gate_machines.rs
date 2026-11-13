//! The gate's platforms on the owner's machines (DESIGN §2.5, r4 §3.8 steps 1 and 5): the
//! machines file and the remote runs of module 3.6, used by `merge`.
//!
//! Without a machines file, the platform this computer runs runs here and every other platform
//! has no machine. With one, each platform's machine is chosen as `atelier test` chooses it
//! ([`machines::candidates`], [`machines::select`]): this computer runs its platform in the gate
//! worktree under the machine lock, through `M`'s pinned atelier; another machine gets `M`
//! pushed and runs the platform's whole `run` there (`remote-run`), and its results must record
//! `M` and `M`'s tree. Every platform runs at the same time: one thread per platform chooses
//! its machine and follows a remote session; the run on this computer happens on the caller's
//! thread, as a step of the gate's run.
//!
//! Step 1's check is lighter than a run: this computer is available for its platform; another
//! machine is when its probe answers ready or busy (a busy one is waited for).
//!
//! A remote machine runs the atelier of the version that starts it (the probe checks it is
//! built from this source), so a merged commit that pins another version cannot run there: the
//! machine is not ready for it.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::bisect::Failures;
use crate::dispatch::VERSION;
use crate::exit::{Failure, Result};
use crate::gate::{self, Availability, GateRun, PlatformOutcome, Platforms};
use crate::git::{self, Checkout};
use crate::jobs::{self, Job};
use crate::machines::{
    self, Attempt, BUSY_POLL, Candidate, MachineLock, MachinesFile, Prober, Skip,
};
use crate::platform::Platform;
use crate::remote::{self, Probe, Remote, Session, SessionEnd, Verdict};
use crate::settings::Settings;
use crate::signals;

/// The owner's machines, as the gate uses them.
pub struct Machines {
    pub home: PathBuf,
    /// `None`: there is no machines file.
    pub file: Option<MachinesFile>,
}

impl Machines {
    /// The owner's machines file in `home`, if there is one; an invalid one refuses (4).
    pub fn load(home: &Path) -> Result<Machines> {
        let path = machines::path_in(home);
        let file = if path.exists() {
            Some(MachinesFile::load(&path)?)
        } else {
            None
        };
        Ok(Machines {
            home: home.to_path_buf(),
            file,
        })
    }

    fn no_file(&self, platform: &str) -> String {
        format!(
            "{platform}: no machine: {} does not exist (it names the machines that run each platform)",
            machines::path_in(&self.home).display()
        )
    }
}

/// What a machine that took a platform hands over.
enum Taken {
    Local(MachineLock),
    Remote(Box<Session>),
}

/// The machine chooser of one platform for `M` in the gate.
struct GateProber<'a> {
    home: &'a Path,
    gate: &'a Checkout,
    settings: &'a Settings,
    m: &'a str,
    job: &'a Job,
    run_dir: &'a Path,
    told: bool,
}

impl Prober for GateProber<'_> {
    type Taken = Taken;

    fn attempt(&mut self, c: &Candidate) -> Attempt<Taken> {
        let platform = self.job.platform.to_string();
        if c.machine.local {
            if !gate::is_local(&platform) {
                return Attempt::NotReady(format!(
                    "this computer is {}, not {platform}",
                    Platform::current()
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "another platform".to_owned())
                ));
            }
            if let Some(first) = jobs::machine_problems(self.gate, self.settings, self.job.platform)
                .into_iter()
                .next()
            {
                return Attempt::NotReady(first);
            }
            let holder = format!("{} (merge gate)", self.gate.root.display());
            return match MachineLock::try_take(self.home, &holder) {
                Ok(Some(l)) => Attempt::Taken(Taken::Local(l)),
                Ok(None) => Attempt::Busy(MachineLock::holder(self.home)),
                Err(f) => Attempt::NotReady(f.message),
            };
        }
        if self.settings.atelier != VERSION {
            return Attempt::NotReady(format!(
                "the merged commit pins atelier {}, and this gate runs {VERSION}: another machine runs the atelier of the version that starts it",
                self.settings.atelier
            ));
        }
        let r = Remote::new(&c.name, &c.machine, self.gate);
        match remote::attempt(
            &r,
            self.gate,
            self.job,
            self.m,
            &remote::log_path(self.run_dir, &c.name),
        ) {
            Attempt::Taken(s) => Attempt::Taken(Taken::Remote(Box::new(s))),
            Attempt::Offline(d) => Attempt::Offline(d),
            Attempt::NotReady(l) => Attempt::NotReady(l),
            Attempt::Busy(h) => Attempt::Busy(h),
        }
    }

    fn stopped(&self) -> bool {
        signals::received().is_some()
    }

    fn pause(&mut self, busy: &[&str]) -> bool {
        if !self.told {
            println!(
                "  {}: waiting for a busy machine to free up: {}",
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

/// What one platform's thread hands back.
enum Chosen {
    /// The outcome, the platform done.
    Done(String, PlatformOutcome),
    /// This computer took it: the caller runs it, holding the lock.
    Local(String, MachineLock),
}

/// The skipped machines, as the gate prints them.
fn skipped_text(skipped: &[(String, Skip)]) -> String {
    if skipped.is_empty() {
        String::new()
    } else {
        format!(" ({})", machines::skips_text(skipped))
    }
}

/// Chooses `platform`'s machine among `cands` and, for another machine, runs it there to its
/// verdict.
#[allow(clippy::too_many_arguments)]
fn choose(
    home: &Path,
    gate: &Checkout,
    settings: &Settings,
    m: &str,
    tree: &str,
    run_dir: &Path,
    platform: &str,
    cands: &[std::result::Result<Candidate, (String, Skip)>],
) -> Chosen {
    let Some(p) = Platform::parse(platform) else {
        return Chosen::Done(
            platform.to_owned(),
            PlatformOutcome::NoMachine(format!("{platform}: not a platform")),
        );
    };
    let job = Job {
        platform: p,
        command: None,
        rel: PathBuf::new(),
    };
    let mut prober = GateProber {
        home,
        gate,
        settings,
        m,
        job: &job,
        run_dir,
        told: false,
    };
    let sel = machines::select(cands, &mut prober);
    let skips = skipped_text(&sel.skipped);
    match sel.taken {
        None => {
            let details = if sel.skipped.is_empty() {
                format!("no machine of {platform} in machines.toml")
            } else {
                sel.skipped
                    .iter()
                    .map(|(n, s)| format!("{n} {}", s.detail()))
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            println!("  {platform}: no machine available{skips}");
            Chosen::Done(
                platform.to_owned(),
                PlatformOutcome::NoMachine(format!("{platform}: {details}")),
            )
        }
        Some((_, Taken::Local(lock))) => {
            if !sel.skipped.is_empty() {
                println!("  {platform}: on this computer{skips}");
            }
            Chosen::Local(platform.to_owned(), lock)
        }
        Some((c, Taken::Remote(session))) => {
            println!("  {platform}: running on {}{skips}", c.name);
            let end = session.finish(m, tree);
            Chosen::Done(platform.to_owned(), remote_outcome(platform, &c.name, end))
        }
    }
}

/// A remote session's verdict as the gate's outcome. A lost connection gives no verdict: the
/// platform had no machine (exit 3), never a pass.
fn remote_outcome(platform: &str, machine: &str, end: SessionEnd) -> PlatformOutcome {
    let run = end.runs.last().map(|(id, _)| id.clone());
    let lines = if end.failures.is_empty() {
        "see its log".to_owned()
    } else {
        end.failures.join("; ")
    };
    match end.verdict {
        Verdict::Passed => {
            println!("  {platform}: PASS on {machine}");
            PlatformOutcome::Passed {
                machine: Some(machine.to_owned()),
                run,
            }
        }
        Verdict::ConnectionLost => {
            println!("  {platform}: connection to {machine} lost");
            PlatformOutcome::NoMachine(format!("{platform}: {lines}"))
        }
        Verdict::Failed | Verdict::Interrupted => {
            let what = format!(
                "on {machine}, run {}: {lines}",
                run.as_deref().unwrap_or("-")
            );
            println!("  {platform}: FAILED ({what})");
            PlatformOutcome::Failed {
                machine: Some(machine.to_owned()),
                run,
                what,
                failures: Failures::whole_on(platform, machine),
            }
        }
    }
}

impl Platforms for Machines {
    fn availability(
        &self,
        checkout: &Checkout,
        settings: &Settings,
        platform: &str,
    ) -> Availability {
        let Some(file) = &self.file else {
            return if gate::is_local(platform) {
                Availability::Local
            } else {
                Availability::None(self.no_file(platform))
            };
        };
        let Some(p) = Platform::parse(platform) else {
            return Availability::None(format!("{platform}: not a platform"));
        };
        if let Err(e) = machines::refuse_windows(p) {
            return Availability::None(e.message);
        }
        let Some(ps) = settings.platforms.get(platform) else {
            return Availability::None(format!("{platform}: not a platform of the settings"));
        };
        let mut why = Vec::new();
        for c in machines::candidates(file, p, ps) {
            match c {
                Err((name, skip)) => why.push(format!("{name} {}", skip.detail())),
                Ok(c) if c.machine.local => {
                    if gate::is_local(platform) {
                        return Availability::Local;
                    }
                    why.push(format!("{} is this computer, not {platform}", c.name));
                }
                Ok(c) => match Remote::new(&c.name, &c.machine, checkout).probe(platform) {
                    Probe::Ready | Probe::Busy(_) => return Availability::Machine(c.name),
                    Probe::Offline(d) => why.push(format!("{} offline ({d})", c.name)),
                    Probe::NotReady(l) => why.push(format!("{} not ready: {l}", c.name)),
                },
            }
        }
        if why.is_empty() {
            why.push(format!("no machine of {platform} in machines.toml"));
        }
        Availability::None(format!("{platform}: {}", why.join("; ")))
    }

    fn run_all(
        &mut self,
        g: &mut GateRun<'_>,
        platforms: &[String],
    ) -> Result<Vec<(String, PlatformOutcome)>> {
        let Some(file) = &self.file else {
            let mut out = Vec::new();
            for p in platforms {
                let o = if gate::is_local(p) {
                    gate::run_local(g, &self.home, p)?
                } else {
                    PlatformOutcome::NoMachine(self.no_file(p))
                };
                out.push((p.clone(), o));
                if g.run.interrupted().is_some() {
                    break;
                }
            }
            return Ok(out);
        };
        let tree = git::run(&g.gate.root, ["rev-parse", &format!("{}^{{tree}}", g.m)])?;
        let run_dir = g.run.dir().to_path_buf();
        let (gate_co, settings, m) = (g.gate, g.settings, g.m);
        let home = self.home.as_path();
        let mut plans = Vec::new();
        for name in platforms {
            let cands = match (Platform::parse(name), settings.platforms.get(name)) {
                (Some(p), Some(ps)) => machines::candidates(file, p, ps),
                _ => Vec::new(),
            };
            plans.push((name.clone(), cands));
        }
        let (tx, rx) = mpsc::channel::<Chosen>();
        let mut results: Vec<(String, PlatformOutcome)> = Vec::new();
        let mut error: Option<Failure> = None;
        std::thread::scope(|s| {
            for (name, cands) in &plans {
                let tx = tx.clone();
                let (tree, run_dir) = (tree.as_str(), run_dir.as_path());
                s.spawn(move || {
                    let chosen = choose(home, gate_co, settings, m, tree, run_dir, name, cands);
                    let _ = tx.send(chosen);
                });
            }
            drop(tx);
            for chosen in rx {
                match chosen {
                    Chosen::Done(p, o) => results.push((p, o)),
                    Chosen::Local(p, lock) => {
                        let o = if error.is_none() {
                            gate::run_local(g, home, &p)
                        } else {
                            Ok(PlatformOutcome::NoMachine(format!("{p}: not run")))
                        };
                        drop(lock);
                        match o {
                            Ok(o) => results.push((p, o)),
                            Err(e) => error = Some(e),
                        }
                    }
                }
            }
        });
        if let Some(e) = error {
            return Err(e);
        }
        results.sort_by_key(|(p, _)| platforms.iter().position(|n| n == p));
        Ok(results)
    }

    fn rerun(&mut self, g: &mut GateRun<'_>, f: &Failures) -> Result<PlatformOutcome> {
        let only = (!f.whole).then_some(&f.only);
        let Some(machine) = &f.machine else {
            let runs_dir = g.ctx.state.runs_dir();
            return gate::run_local_in(
                g.run,
                &runs_dir,
                &g.gate.root,
                g.settings,
                &self.home,
                &f.platform,
                only,
            );
        };
        let Some(file) = &self.file else {
            return Ok(PlatformOutcome::NoMachine(self.no_file(&f.platform)));
        };
        let (Some(p), Some(ps)) = (
            Platform::parse(&f.platform),
            g.settings.platforms.get(&f.platform),
        ) else {
            return Ok(PlatformOutcome::NoMachine(format!(
                "{}: not a platform of the prefix's settings",
                f.platform
            )));
        };
        // The machine it failed on first, then the platform's others in their order.
        let mut cands = machines::candidates(file, p, ps);
        cands.sort_by_key(|c| !matches!(c, Ok(c) if c.name == *machine));
        let tree = git::run(&g.gate.root, ["rev-parse", &format!("{}^{{tree}}", g.m)])?;
        let run_dir = g.run.dir().to_path_buf();
        match choose(
            &self.home,
            g.gate,
            g.settings,
            g.m,
            &tree,
            &run_dir,
            &f.platform,
            &cands,
        ) {
            Chosen::Done(_, o) => Ok(o),
            Chosen::Local(p, lock) => {
                let o = gate::run_local(g, &self.home, &p);
                drop(lock);
                o
            }
        }
    }
}
