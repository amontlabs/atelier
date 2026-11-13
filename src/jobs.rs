//! A platform's job on the machine that runs it (DESIGN §3 `test`, §2.3), the same code on this
//! computer and, through `remote-run`, on another one.
//!
//! Before every run, full or narrow: the `check.prepare` hooks that apply to the platform, then
//! the platform's own `prepare`. Then the given command (a narrow run), in the caller's folder
//! relative to the checkout root, or the platform's whole `run`. Every step runs in its own
//! process group with the run markers and `NEXTEST_RETRIES=0 NEXTEST_FLAKY_RESULT=fail`. The
//! tree must not change: it is compared after each preparation step and at the end. A failing
//! preparation step stops the run.
//!
//! A platform whose `run` is `"check"` runs the full `atelier check` (module 3.2), which runs the
//! `check.prepare` hooks itself: the job's own run then holds only the platform's `prepare`, and
//! the caller runs the check after finishing it ([`Ending::ThenCheck`]).
//!
//! The machine preflight ([`machine_problems`]) is what a machine must meet before it takes a
//! run: its platform, then `atelier check`'s own prerequisites ([`crate::prereq::verify`]) for the
//! files of the commit it runs (r4 §4.4, §6).

use std::path::PathBuf;

use crate::check;
use crate::exit::{Exit, Failure, Result};
use crate::git::Checkout;
use crate::platform::Platform;
use crate::preflight::Context;
use crate::prereq;
use crate::runs::{self, Run, RunKind, StepStatus};
use crate::settings::{PlatformRun, PlatformSettings, Settings};
use crate::tree;

/// The environment every step of a platform job gets (r4 §3.7).
pub const STEP_ENV: &[(&str, &str)] = &[("NEXTEST_RETRIES", "0"), ("NEXTEST_FLAKY_RESULT", "fail")];

/// What one platform job runs.
#[derive(Debug, Clone)]
pub struct Job {
    pub platform: Platform,
    /// A narrow run's command; `None` runs the platform's whole `run`.
    pub command: Option<Vec<String>>,
    /// The caller's folder relative to the checkout root; the command runs there.
    pub rel: PathBuf,
}

impl Job {
    pub fn kind(&self) -> RunKind {
        if self.command.is_some() {
            RunKind::TestNarrow
        } else {
            RunKind::TestFull
        }
    }

    /// The command as people read it, for `run.toml`.
    pub fn describe(&self) -> String {
        match &self.command {
            Some(c) => format!("test {} -- {}", self.platform, c.join(" ")),
            None => format!("test {}", self.platform),
        }
    }
}

/// How a job's own run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// The run is complete; true when every step passed.
    Done(bool),
    /// The preparation passed and the platform runs `atelier check`: finish this run, then run
    /// the check ([`full_check`]).
    ThenCheck,
}

/// The settings of the job's platform, or a refusal (4) naming the platforms.
pub fn platform_settings(settings: &Settings, platform: Platform) -> Result<&PlatformSettings> {
    settings
        .platforms
        .get(&platform.to_string())
        .ok_or_else(|| {
            Failure::refused(format!(
                "{platform} is not a platform of this project (it has: {})",
                settings
                    .platforms
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
}

/// Runs the job's steps in `run`, in the checkout of `ctx`.
pub fn run_steps(ctx: &Context, run: &mut Run, job: &Job) -> Result<Ending> {
    let root = ctx.checkout.root.clone();
    let ps = platform_settings(&ctx.settings, job.platform)?;
    let env: Vec<(String, String)> = STEP_ENV
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    let then_check = job.command.is_none() && matches!(ps.run, PlatformRun::Named(_));

    let before = match tree::tree_id(&ctx.checkout) {
        Ok(t) => t,
        Err(e) => {
            run.fail(e.message.clone());
            return Ok(Ending::Done(false));
        }
    };
    let head = ctx.checkout.head_commit()?;
    let head_tree = tree::head_tree(&ctx.checkout)?;
    run.set_tree(
        before.clone(),
        head,
        head_tree.as_deref() != Some(before.as_str()),
    );

    let mut prepare: Vec<(String, Vec<String>)> = Vec::new();
    if !then_check {
        for h in &ctx.settings.check.prepare {
            let applies = h
                .platforms
                .as_ref()
                .is_none_or(|ps| ps.iter().any(|p| *p == job.platform.to_string()));
            if applies {
                prepare.push((format!("prepare-{}", h.name), h.run.clone()));
            }
        }
    }
    if let Some(argv) = &ps.prepare {
        prepare.push(("prepare".to_owned(), argv.clone()));
    }
    for (name, argv) in &prepare {
        let rec = run.step(name, argv, &root, &env)?;
        if !rec.passed() {
            return Ok(Ending::Done(false));
        }
        match tree::tree_id(&ctx.checkout) {
            Ok(t) if t == before => {}
            Ok(_) => {
                run.fail(format!(
                    "{name} changed the tree: preparation may write only git-ignored files"
                ));
                return Ok(Ending::Done(false));
            }
            Err(e) => {
                run.fail(e.message);
                return Ok(Ending::Done(false));
            }
        }
    }
    if then_check {
        return Ok(Ending::ThenCheck);
    }

    let rec = match (&job.command, &ps.run) {
        (Some(argv), _) => {
            let cwd = root.join(&job.rel);
            if !cwd.is_dir() {
                run.fail(format!(
                    "{} does not exist in this checkout",
                    job.rel.display()
                ));
                return Ok(Ending::Done(false));
            }
            run.step("command", argv, &cwd, &env)?
        }
        (None, PlatformRun::Command(argv)) => run.step("run", argv, &root, &env)?,
        (None, PlatformRun::Named(_)) => unreachable!("handled by then_check"),
    };
    let mut passed = rec.passed();
    if rec.status != StepStatus::Interrupted {
        match tree::tree_id(&ctx.checkout) {
            Ok(t) if t == before => {}
            Ok(t) => {
                run.fail(format!("the run changed the tree ({before} → {t})"));
                passed = false;
            }
            Err(e) => {
                run.fail(e.message);
                passed = false;
            }
        }
    }
    Ok(Ending::Done(passed))
}

/// The full `atelier check` of the checkout (module 3.2): its own run (kind `check`), its own
/// lock, its own report.
pub fn full_check(ctx: &Context) -> Result<Exit> {
    check::run(ctx, false)
}

/// `<os>-<arch>` of the machine this program runs on, from `uname`.
pub fn uname_platform() -> std::result::Result<String, String> {
    // SAFETY: `uname` fills the zeroed struct; its fields are NUL-terminated.
    let (sys, machine) = unsafe {
        let mut u: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut u) != 0 {
            return Err(format!("uname: {}", std::io::Error::last_os_error()));
        }
        (
            std::ffi::CStr::from_ptr(u.sysname.as_ptr())
                .to_string_lossy()
                .into_owned(),
            std::ffi::CStr::from_ptr(u.machine.as_ptr())
                .to_string_lossy()
                .into_owned(),
        )
    };
    let os = match sys.as_str() {
        "Darwin" => "macos",
        "Linux" => "linux",
        other => return Err(format!("uname says {other} {machine}, not macOS or Linux")),
    };
    let arch = match machine.as_str() {
        "arm64" | "aarch64" => "aarch64",
        "x86_64" | "amd64" => "x86_64",
        other => return Err(format!("uname says {sys} {other}: not aarch64 or x86_64")),
    };
    Ok(format!("{os}-{arch}"))
}

/// The machine preflight, one line per problem (the first is the `not ready` line): the
/// platform from `uname` is the declared one, then the prerequisites of `atelier check`
/// ([`prereq::verify`]: toolchains, nextest, declared tools, nextest files, mbx) for the files
/// of `checkout` as they are.
pub fn machine_problems(
    checkout: &Checkout,
    settings: &Settings,
    declared: Platform,
) -> Vec<String> {
    match uname_platform() {
        Ok(p) if p == declared.to_string() => {}
        Ok(p) => {
            return vec![format!(
                "uname says {p}; the machine is declared {declared}"
            )];
        }
        Err(e) => return vec![e],
    }
    let marker = match runs::new_run_id() {
        Ok(m) => m,
        Err(f) => return vec![f.message],
    };
    let workspaces = prereq::workspaces(checkout, settings, &marker);
    match prereq::verify(checkout, settings, &workspaces, Some(declared), &marker) {
        Ok(_) => Vec::new(),
        Err(f) => {
            let mut lines = f.message.lines();
            let first = lines.next().unwrap_or_default();
            if first == PREREQ_HEAD {
                lines.map(|l| l.trim().to_owned()).collect()
            } else {
                vec![f.message.clone()]
            }
        }
    }
}

/// The first line of [`prereq::verify`]'s refusal; the problems follow, one per line.
const PREREQ_HEAD: &str = "prerequisites not met:";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uname_names_this_platform() {
        assert_eq!(
            uname_platform().unwrap(),
            Platform::current().unwrap().to_string()
        );
    }

    #[test]
    fn the_preflight_names_a_wrong_platform_and_each_prerequisite_problem() {
        let here = Platform::current().unwrap();
        let other = if here.to_string() == "linux-x86_64" {
            "macos-aarch64"
        } else {
            "linux-x86_64"
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let g = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(root)
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .status()
                    .unwrap()
                    .success()
            )
        };
        g(&["init", "-q"]);
        let checkout = Checkout::find(root).unwrap().unwrap();
        let settings = Settings::parse(&format!(
            "atelier = \"0.1.0\"\n[platforms.{here}]\nrun = \"check\"\n[platforms.{other}]\nrun = \"check\"\n\
             [[prerequisites.tools]]\nname = \"sh\"\nrun = [\"sh\", \"-c\", \"echo v1.2\"]\nexpect = \"v2.\"\n\
             [[prerequisites.tools]]\nname = \"also\"\nrun = [\"sh\", \"-c\", \"echo v0\"]\nexpect = \"v1\"\n\
             [[prerequisites.tools]]\nname = \"elsewhere\"\nrun = [\"false\"]\nexpect = \"x\"\nplatforms = [\"{other}\"]\n"
        ))
        .unwrap();
        let p = machine_problems(&checkout, &settings, here);
        assert_eq!(p.len(), 2, "{p:?}");
        assert!(p[0].starts_with("tool sh: "), "{p:?}");
        assert!(p[1].starts_with("tool also: "), "{p:?}");
        let p = machine_problems(&checkout, &settings, Platform::parse(other).unwrap());
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].starts_with("uname says "), "{p:?}");
    }
}
