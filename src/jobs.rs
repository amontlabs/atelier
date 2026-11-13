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
//! The machine preflight ([`machine_problems`]) is the part of the prerequisites a machine must
//! meet before it takes a run: its platform, the declared tools, and the mbx rules (r4 §4.4, §6).

use std::path::{Path, PathBuf};

use crate::exit::{Exit, Failure, Result};
use crate::platform::Platform;
use crate::preflight::Context;
use crate::runs::{Run, RunKind, StepStatus};
use crate::settings::{PlatformRun, PlatformSettings, Settings};
use crate::tree;

/// The environment every step of a platform job gets (r4 §3.7).
pub const STEP_ENV: &[(&str, &str)] = &[("NEXTEST_RETRIES", "0"), ("NEXTEST_FLAKY_RESULT", "fail")];

/// Variables that make cargo bypass mbx (r4 §6).
pub const MBX_BYPASS_VARS: &[&str] = &["MBX_DISABLE", "CARGO_TARGET_DIR", "RUSTC_WRAPPER"];

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

/// The full `atelier check` of the checkout (module 3.2); its own run, its own lock.
pub fn full_check(_ctx: &Context) -> Result<Exit> {
    Err(Failure::usage(
        "the platform's run is \"check\": check: not built yet (module 3.2)",
    ))
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
/// platform from `uname` is the declared one; each declared tool that applies to the platform
/// answers as expected; with `[prerequisites.mbx]`, nothing makes cargo bypass mbx, the first
/// `cargo` on `PATH` is one of the listed files, no `PATH` entry is relative, mbx is the pinned
/// version, and each workspace's `target` is absent or a link into mbx's storage.
pub fn machine_problems(settings: &Settings, declared: Platform, root: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    match uname_platform() {
        Ok(p) if p == declared.to_string() => {}
        Ok(p) => problems.push(format!(
            "uname says {p}; the machine is declared {declared}"
        )),
        Err(e) => problems.push(e),
    }
    for tool in &settings.prerequisites.tools {
        let applies = tool
            .platforms
            .as_ref()
            .is_none_or(|ps| ps.iter().any(|p| *p == declared.to_string()));
        if !applies {
            continue;
        }
        match first_line(root, &tool.run) {
            Ok(line) if line.starts_with(&tool.expect) => {}
            Ok(line) => problems.push(format!(
                "{}: `{}` printed {line:?}; expected it to start with {:?}",
                tool.name,
                tool.run.join(" "),
                tool.expect
            )),
            Err(e) => problems.push(format!("{}: `{}` {e}", tool.name, tool.run.join(" "))),
        }
    }
    if let Some(mbx) = &settings.prerequisites.mbx {
        for var in MBX_BYPASS_VARS {
            if std::env::var_os(var).is_some() {
                problems.push(format!("{var} is set: it makes cargo bypass mbx; unset it"));
            }
        }
        let path = std::env::var("PATH").unwrap_or_default();
        if let Some(rel) = path.split(':').find(|p| !p.starts_with('/')) {
            problems.push(format!("PATH has a relative entry {rel:?}"));
        }
        let listed: Vec<PathBuf> = mbx.cargo.iter().map(|c| expand_home(c)).collect();
        match first_on_path(&path, "cargo") {
            Some(found) if listed.contains(&found) => {}
            Some(found) => problems.push(format!(
                "the first cargo on PATH is {}; mbx needs one of: {}",
                found.display(),
                mbx.cargo.join(", ")
            )),
            None => problems.push("no cargo on PATH".to_owned()),
        }
        match first_line(root, &["mbx".to_owned(), "--version".to_owned()]) {
            Ok(line) if line.split_whitespace().any(|w| w == mbx.version) => {}
            Ok(line) => problems.push(format!(
                "mbx --version printed {line:?}; the settings pin {}",
                mbx.version
            )),
            Err(e) => problems.push(format!("mbx --version {e}")),
        }
        for ws in &settings.workspaces {
            if let Some(p) = target_problem(root, ws) {
                problems.push(p);
            }
        }
    }
    problems
}

/// A workspace's `target` must be absent or a link into mbx's storage: a real folder is plain
/// cargo's, and mixing the two fails on Linux (r4 §6, E7).
fn target_problem(root: &Path, ws: &str) -> Option<String> {
    let target = root.join(ws).join("target");
    let shown = Path::new(ws).join("target");
    let meta = std::fs::symlink_metadata(&target).ok()?;
    if !meta.file_type().is_symlink() {
        return Some(format!(
            "{} is a real folder that plain cargo made; mbx needs it absent or a link into its storage: rm -rf {}",
            shown.display(),
            target.display()
        ));
    }
    let to = std::fs::read_link(&target).ok()?;
    if to.components().any(|c| c.as_os_str() == "mbx") {
        None
    } else {
        Some(format!(
            "{} links to {}, which is not mbx's storage",
            shown.display(),
            to.display()
        ))
    }
}

/// `~/x` → `$HOME/x`.
pub fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(p),
    }
}

/// The first executable file `name` in a `PATH` value.
pub fn first_on_path(path: &str, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    path.split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(name))
        .find(|p| {
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// Runs a prerequisite command in `root` and returns the first line of its output (stdout, or
/// stderr when stdout is empty).
fn first_line(root: &Path, argv: &[String]) -> std::result::Result<String, String> {
    let (program, args) = argv.split_first().ok_or("is empty")?;
    let out = std::process::Command::new(program)
        .args(args)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("cannot run: {e}"))?;
    if !out.status.success() {
        return Err(format!("failed ({})", out.status));
    }
    let text = if out.stdout.iter().all(u8::is_ascii_whitespace) {
        out.stderr
    } else {
        out.stdout
    };
    Ok(String::from_utf8_lossy(&text)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_owned())
}

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
    fn the_preflight_names_a_wrong_platform_and_a_wrong_tool() {
        let here = Platform::current().unwrap();
        let other = if here.to_string() == "linux-x86_64" {
            "macos-aarch64"
        } else {
            "linux-x86_64"
        };
        let settings = Settings::parse(&format!(
            "atelier = \"0.1.0\"\n[platforms.{here}]\nrun = \"check\"\n[platforms.{other}]\nrun = \"check\"\n\
             [[prerequisites.tools]]\nname = \"sh\"\nrun = [\"sh\", \"-c\", \"echo v1.2\"]\nexpect = \"v2.\"\n\
             [[prerequisites.tools]]\nname = \"elsewhere\"\nrun = [\"false\"]\nexpect = \"x\"\nplatforms = [\"{other}\"]\n"
        ))
        .unwrap();
        let root = std::env::temp_dir();
        let p = machine_problems(&settings, here, &root);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(
            p[0].starts_with("sh: `sh -c echo v1.2` printed \"v1.2\""),
            "{p:?}"
        );
        let p = machine_problems(&settings, Platform::parse(other).unwrap(), &root);
        assert!(p[0].starts_with("uname says "), "{p:?}");
        assert!(p.iter().any(|l| l.starts_with("elsewhere: ")), "{p:?}");
    }

    #[test]
    fn a_real_target_folder_is_refused_and_an_mbx_link_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(ws.join("target")).unwrap();
        assert!(
            target_problem(dir.path(), "ws")
                .unwrap()
                .contains("real folder")
        );
        std::fs::remove_dir(ws.join("target")).unwrap();
        assert!(target_problem(dir.path(), "ws").is_none());
        std::os::unix::fs::symlink("/home/u/.cache/mbx/targets/x", ws.join("target")).unwrap();
        assert!(target_problem(dir.path(), "ws").is_none());
        std::fs::remove_file(ws.join("target")).unwrap();
        std::os::unix::fs::symlink("/elsewhere/x", ws.join("target")).unwrap();
        assert!(
            target_problem(dir.path(), "ws")
                .unwrap()
                .contains("not mbx's storage")
        );
    }

    #[test]
    fn first_on_path_skips_folders_without_the_program() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let tool = b.join("cargo");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!("{}:{}", a.display(), b.display());
        assert_eq!(first_on_path(&path, "cargo"), Some(tool));
    }
}
