//! `atelier check [--compile-only]` (DESIGN §3, r4 §3.7): proves the tree in this worktree.
//!
//! Order: prerequisites; this worktree's run lock; the tree id; the `check.prepare` hooks (the
//! tree must not change after each, and a failing or tree-changing hook stops the run); format,
//! compile, test (nextest with fixed flags); the project steps; the tree id again, the `.mbx.toml`
//! and build-folder checks again, and the survivor scan. A failing step does not stop later
//! independent ones; a compile failure skips that workspace's tests.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::exit::{Exit, Result};
use crate::git::{self, Checkout};
use crate::locks::LockKey;
use crate::nextest::{self, Verdict};
use crate::platform::Platform;
use crate::preflight::Context;
use crate::prereq::{self, Workspace};
use crate::runs::{Outcome, Run, RunResult, StepStatus};
use crate::tree;

/// Runs the check; the exit code is 0 for GREEN (or `compile: OK`) and 1 for RED.
pub fn run(ctx: &Context, compile_only: bool) -> Result<Exit> {
    let checkout = &ctx.checkout;
    let workspaces = prereq::workspaces(checkout, &ctx.settings);
    let platform = Platform::current();
    let snapshot = prereq::verify(checkout, &ctx.settings, &workspaces, platform)?;

    let started = Instant::now();
    let mut run = Run::start(
        &ctx.state,
        LockKey::worktree(checkout),
        "check",
        &checkout.root,
    )?;
    let run_id = run.id().to_owned();
    let run_dir = run.dir().to_path_buf();

    let before = match tree::tree_id(checkout) {
        Ok(t) => t,
        Err(e) => {
            run.fail(e.message.clone());
            let _ = run.finish(false);
            return Err(e);
        }
    };
    let head = checkout.head_commit()?;
    let head_tree = tree::head_tree(checkout)?;
    let uncommitted = head_tree.as_deref() != Some(before.as_str());
    run.set_tree(before.clone(), head.clone(), uncommitted);

    let mut c = Checker {
        ctx,
        run,
        workspaces: &workspaces,
        platform,
        before,
        extras: Vec::new(),
        stop: false,
        tests: 0,
        excluded: 0,
        compile_only,
    };
    c.hooks()?;
    if !c.stop {
        c.cargo_steps()?;
    }
    if !c.stop {
        c.project_steps()?;
    }
    c.end(&snapshot);

    let Checker {
        run,
        extras,
        tests,
        excluded,
        ..
    } = c;
    let result = run.finish(extras.is_empty())?;
    let elapsed = started.elapsed();
    Ok(report(
        &result,
        &run_dir,
        &run_id,
        &extras,
        tests,
        excluded,
        elapsed,
        compile_only,
    ))
}

struct Checker<'a> {
    ctx: &'a Context,
    run: Run,
    workspaces: &'a [Workspace],
    platform: Option<Platform>,
    before: String,
    /// Failures that are not a step's own exit: a hook that changed the tree, a changed `.mbx.toml`.
    extras: Vec<String>,
    stop: bool,
    tests: usize,
    excluded: usize,
    compile_only: bool,
}

impl Checker<'_> {
    fn checkout(&self) -> &Checkout {
        &self.ctx.checkout
    }

    /// Environment of every job: no retries, whatever is exported in the caller's shell.
    fn env(extra: &[(&str, &str)]) -> Vec<(String, String)> {
        nextest::JOB_ENV
            .iter()
            .chain(extra)
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// Runs one step and prints its verdict; returns whether it passed. A stop signal ends the run.
    fn step(
        &mut self,
        name: &str,
        argv: &[String],
        cwd: &Path,
        stdout: Option<&str>,
    ) -> Result<bool> {
        let env = if stdout.is_some() {
            Self::env(nextest::JSON_ENV)
        } else {
            Self::env(&[])
        };
        let rec = self.run.step_with_stdout(name, argv, cwd, &env, stdout)?;
        println!(
            "  {name}: {}",
            match rec.status {
                StepStatus::Passed => "ok".to_owned(),
                StepStatus::Failed => format!("FAILED (log {})", rec.log),
                StepStatus::LeftProcesses => format!("FAILED, left processes behind ({})", rec.log),
                StepStatus::Interrupted => "interrupted".to_owned(),
                StepStatus::NotStarted => format!("could not start ({})", rec.log),
            }
        );
        if self.run.interrupted().is_some() {
            self.stop = true;
        }
        Ok(rec.passed())
    }

    fn fail(&mut self, name: &str, why: String) {
        println!("  {name}: {why}");
        self.run.fail(why);
        if !self.extras.iter().any(|e| e == name) {
            self.extras.push(name.to_owned());
        }
    }

    /// The tree id now, compared with the one recorded at the start. `Some(files)` when it differs.
    fn tree_changed(&self) -> Result<Option<String>> {
        let now = tree::tree_id(self.checkout())?;
        if now == self.before {
            return Ok(None);
        }
        let files = git::run(
            &self.checkout().root,
            ["diff-tree", "-r", "--name-only", &self.before, &now],
        )
        .unwrap_or_default();
        let files: Vec<&str> = files.lines().take(20).collect();
        Ok(Some(files.join(", ")))
    }

    fn hooks(&mut self) -> Result<()> {
        let root = self.checkout().root.clone();
        let hooks: Vec<_> = self
            .ctx
            .settings
            .check
            .prepare
            .iter()
            .filter(|h| applies(h.platforms.as_deref(), self.platform))
            .cloned()
            .collect();
        for h in hooks {
            let name = format!("prepare:{}", h.name);
            if !self.step(&name, &h.run, &root, None)? {
                self.fail(&name, format!("hook {} failed; the run stops here", h.name));
                self.stop = true;
                return Ok(());
            }
            if self.stop {
                return Ok(());
            }
            match self.tree_changed() {
                Ok(None) => {}
                Ok(Some(files)) => {
                    self.fail(
                        &name,
                        format!(
                            "hook {} changed the tree ({files}); the run stops here",
                            h.name
                        ),
                    );
                    self.stop = true;
                    return Ok(());
                }
                Err(e) => {
                    self.fail(&name, format!("hook {}: {}", h.name, e.message));
                    self.stop = true;
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn suffixed(&self, base: &str, ws: &Workspace) -> String {
        if self.workspaces.len() > 1 {
            format!("{base}@{}", ws.rel)
        } else {
            base.to_owned()
        }
    }

    fn cargo_steps(&mut self) -> Result<()> {
        let workspaces = self.workspaces.to_vec();
        let mut healthy = vec![true; workspaces.len()];

        for (i, ws) in workspaces.iter().enumerate() {
            let name = self.suffixed("fmt", ws);
            if let Some(why) = &ws.metadata_error {
                self.fail(&name, format!("workspace {}: {why}", ws.rel));
                healthy[i] = false;
                continue;
            }
            let mut argv: Vec<String> = ["cargo", "fmt", "--check"].map(str::to_owned).to_vec();
            for m in &ws.members {
                argv.push("-p".to_owned());
                argv.push(m.clone());
            }
            self.step(&name, &argv, &ws.dir, None)?;
            if self.stop {
                return Ok(());
            }
        }

        let mut compiled = vec![false; workspaces.len()];
        for (i, ws) in workspaces.iter().enumerate() {
            if !healthy[i] {
                continue;
            }
            let name = self.suffixed("compile", ws);
            let argv: Vec<String> = ["cargo", "check", "--workspace", "--all-targets", "--locked"]
                .map(str::to_owned)
                .to_vec();
            compiled[i] = self.step(&name, &argv, &ws.dir, None)?;
            if self.stop {
                return Ok(());
            }
        }
        if self.compile_only {
            return Ok(());
        }

        let exclude = self
            .ctx
            .settings
            .rust
            .as_ref()
            .map(|r| r.exclude.clone())
            .unwrap_or_default();
        for (i, ws) in workspaces.iter().enumerate() {
            let name = self.suffixed("tests", ws);
            if !healthy[i] {
                println!("  {name}: skipped (cargo metadata failed)");
                continue;
            }
            if !compiled[i] {
                println!("  {name}: skipped (compile failed)");
                continue;
            }
            for e in &exclude {
                println!("  excluded: {} ({})", e.filter, e.reason);
            }
            if !exclude.is_empty() {
                // The tests the run leaves out: what the exclusion filtersets match.
                let any = exclude
                    .iter()
                    .map(|e| format!("({})", e.filter))
                    .collect::<Vec<_>>()
                    .join(" | ");
                let mut argv: Vec<String> = [
                    "cargo",
                    "nextest",
                    "list",
                    "--workspace",
                    "--locked",
                    "--ignore-default-filter",
                    "--message-format",
                    "json",
                    "-E",
                ]
                .map(str::to_owned)
                .to_vec();
                argv.push(any);
                let file = format!("excluded-{}.json", file_tag(ws));
                if self.step(&format!("{name}:exclusions"), &argv, &ws.dir, Some(&file))? {
                    let text = fs::read_to_string(self.run.dir().join(&file)).unwrap_or_default();
                    self.excluded += nextest::count_matching(&text);
                }
            }
            let mut argv = vec!["cargo".to_owned()];
            argv.extend(nextest::run_args(&exclude));
            let file = format!("tests-{}.jsonl", file_tag(ws));
            let ok = self.step(&name, &argv, &ws.dir, Some(&file))?;
            let stream = fs::read_to_string(self.run.dir().join(&file)).unwrap_or_default();
            let results = nextest::parse_results(&stream);
            self.tests += results
                .iter()
                .filter(|r| r.verdict != Verdict::Ignored)
                .count();
            let mut tsv = String::new();
            for r in &results {
                tsv.push_str(&format!(
                    "{}\t{}\t{}\t{}\n",
                    ws.rel,
                    r.name,
                    r.verdict.word(),
                    r.seconds.map(|s| format!("{s:.3}")).unwrap_or_default()
                ));
                if r.verdict == Verdict::Failed {
                    println!("    failed test: {}", r.name);
                }
            }
            append(&self.run.dir().join("tests.tsv"), &tsv);
            if ok && results.is_empty() {
                self.fail(
                    &name,
                    "nextest passed but reported no test results".to_owned(),
                );
            }
            if self.stop {
                return Ok(());
            }
        }
        Ok(())
    }

    fn project_steps(&mut self) -> Result<()> {
        let root = self.checkout().root.clone();
        let steps: Vec<_> = self
            .ctx
            .settings
            .check
            .steps
            .iter()
            .filter(|s| !self.compile_only || s.compile)
            .cloned()
            .collect();
        for s in steps {
            self.step(&s.name, &s.run, &root, None)?;
            if self.stop {
                break;
            }
        }
        Ok(())
    }

    /// Step 8: the tree, the mbx files, then (in `finish`) the survivor scan.
    fn end(&mut self, snapshot: &prereq::Snapshot) {
        if !self.extras.iter().any(|e| e.starts_with("prepare:")) {
            match self.tree_changed() {
                Ok(None) => {}
                Ok(Some(files)) => {
                    self.fail("tree", format!("the tree changed during the run ({files})"))
                }
                Err(e) => self.fail("tree", e.message),
            }
        }
        let lines = prereq::recheck(
            self.checkout(),
            &self.ctx.settings,
            self.workspaces,
            snapshot,
        );
        for line in lines {
            self.fail("mbx", line);
        }
    }
}

fn applies(platforms: Option<&[String]>, platform: Option<Platform>) -> bool {
    match (platforms, platform) {
        (None, _) => true,
        (Some(list), Some(p)) => list.iter().any(|l| Platform::parse(l) == Some(p)),
        (Some(_), None) => false,
    }
}

/// A workspace folder as part of a file name.
fn file_tag(ws: &Workspace) -> String {
    let t = ws.tag();
    if t.is_empty() { "root".to_owned() } else { t }
}

fn append(path: &Path, text: &str) {
    use std::io::Write;
    if text.is_empty() {
        return;
    }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(text.as_bytes());
    }
}

/// `7m02s`, `42s`.
pub fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// The failed names, in the order they happened: steps first, then the other failures.
fn failed_names(result: &RunResult, extras: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut add = |n: &str| {
        let n = n.to_owned();
        if !names.contains(&n) {
            names.push(n);
        }
    };
    for s in &result.steps {
        if !matches!(s.status, StepStatus::Passed | StepStatus::Interrupted) {
            add(&s.name);
        }
    }
    for e in extras {
        add(e);
    }
    if !result.survivors.is_empty() {
        add("left processes behind");
    }
    names
}

#[allow(clippy::too_many_arguments)]
fn report(
    result: &RunResult,
    run_dir: &Path,
    run_id: &str,
    extras: &[String],
    tests: usize,
    excluded: usize,
    elapsed: Duration,
    compile_only: bool,
) -> Exit {
    for f in &result.failures {
        println!("  {f}");
    }
    let commit = result
        .commit
        .as_deref()
        .map(|c| c.chars().take(7).collect::<String>())
        .unwrap_or_else(|| "none".to_owned());
    let state = if result.uncommitted == Some(true) {
        "+ uncommitted changes"
    } else {
        "clean"
    };
    let head = format!("run {run_id}, commit {commit} ({state})");
    let log = run_dir.display();
    let word = if compile_only { "compile" } else { "check" };
    if result.outcome == Outcome::Interrupted {
        println!(
            "{word}: INTERRUPTED — {head}, {}, log {log}",
            result.signal.as_deref().unwrap_or("stopped")
        );
        return Exit::Fail;
    }
    if result.outcome == Outcome::Passed {
        if compile_only {
            println!("compile: OK — {head}, {}, log {log}", duration(elapsed));
        } else {
            println!(
                "check: GREEN — {head}, {tests} tests, {excluded} excluded, {}, log {log}",
                duration(elapsed)
            );
        }
        return Exit::Ok;
    }
    let failed = failed_names(result, extras).join(", ");
    if compile_only {
        println!("compile: FAILED — {head}, failed: {failed}; log {log}");
    } else {
        println!("check: RED — {head}, failed: {failed}; log {log}");
    }
    Exit::Fail
}
