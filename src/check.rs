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
use crate::locks::{HeldLock, LockKey};
use crate::nextest::{self, Verdict};
use crate::platform::Platform;
use crate::preflight::Context;
use crate::prereq::{self, Workspace};
use crate::runs::{self, Outcome, Run, RunKind, RunResult, StepStatus, TestCounts};
use crate::settings::Exclude;
use crate::tree;

/// The per-test results in the run folder: workspace, test, verdict, seconds, tab-separated.
const TEST_RESULTS: &str = "tests.tsv";

/// What a narrow re-run (`atelier check --only <file>`, used by the gate's bisection, DESIGN
/// §8.3) runs: the `check.prepare` hooks, then only these tests by name and these steps whole.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Only {
    #[serde(default)]
    pub tests: Vec<OnlyTest>,
    /// Step names as a check's result names them (`fmt`, `compile@w`, a project step).
    #[serde(default)]
    pub steps: Vec<String>,
}

/// One test of [`Only`]: its workspace folder and nextest's name (`<binary id>$<test name>`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OnlyTest {
    pub workspace: String,
    pub name: String,
}

impl Only {
    pub fn read(path: &Path) -> Result<Only> {
        let text = fs::read_to_string(path)
            .map_err(|e| crate::exit::Failure::usage(format!("--only {}: {e}", path.display())))?;
        toml::from_str(&text).map_err(|e| {
            crate::exit::Failure::usage(format!(
                "--only {}: {}",
                path.display(),
                e.to_string().trim()
            ))
        })
    }

    fn tests_in(&self, ws: &Workspace) -> Vec<&OnlyTest> {
        self.tests
            .iter()
            .filter(|t| t.workspace == ws.rel)
            .collect()
    }

    /// The nextest filterset selecting these tests by test name. The libtest-JSON name's prefix
    /// (`<crate>::<binary>`) is not nextest's binary id (`nx::nx` for the library `nx`,
    /// `nx::tool` for `nx::bin/tool`), so the run may also select same-named tests of other
    /// binaries; only the listed names are judged.
    fn filterset(tests: &[&OnlyTest]) -> String {
        tests
            .iter()
            .map(|t| {
                let test = t.name.split_once('$').map_or(t.name.as_str(), |(_, t)| t);
                format!("test(={test})")
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }
}

/// Runs the check; the exit code is 0 for GREEN (or `compile: OK`) and 1 for RED.
pub fn run(ctx: &Context, compile_only: bool) -> Result<Exit> {
    run_mode(ctx, compile_only, None, &mut None)
}

/// The branch checked out here when atelier has a record of it.
fn started_branch(ctx: &Context) -> Result<Option<String>> {
    let out = crate::git::output(
        &ctx.checkout.root,
        ["symbolic-ref", "--no-recurse", "-q", "HEAD"],
    )?;
    let full = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let Some(branch) = full
        .strip_prefix("refs/heads/")
        .filter(|_| out.status.success())
    else {
        return Ok(None);
    };
    Ok(crate::records::BranchRecord::read(&ctx.state, branch)?.map(|_| branch.to_owned()))
}

/// As [`run`], and sets `id` to the check's run id once the run has started.
pub fn run_noting(ctx: &Context, compile_only: bool, id: &mut Option<String>) -> Result<Exit> {
    run_mode(ctx, compile_only, None, id)
}

/// [`run`], or with `only` the narrow re-run of [`Only`]: 0 when none of the given failures
/// appears, 1 otherwise; never GREEN. Sets `id` to the check's run id once the run has started.
pub fn run_mode(
    ctx: &Context,
    compile_only: bool,
    only: Option<Only>,
    id: &mut Option<String>,
) -> Result<Exit> {
    let checkout = &ctx.checkout;
    // In the worktree of a branch atelier started, the branch lock is held shared for the whole
    // check, so `clean`, `follow` and `start` refuse while it runs (r4 §3.9 step 1).
    let _branch_lock = match started_branch(ctx)? {
        Some(b) => Some(HeldLock::acquire_shared(&ctx.state, LockKey::branch(&b))?),
        None => None,
    };
    // The prerequisite commands carry the id the run then takes, so any of them that leaves a
    // process behind is found by the marker scan.
    let marker = match std::env::var(runs::RUN_ID_VAR) {
        Ok(id) if runs::is_run_id(&id) && !ctx.state.runs_dir().join(&id).exists() => id,
        _ => runs::new_run_id()?,
    };
    let workspaces = prereq::workspaces(checkout, &ctx.settings, &marker);
    let platform = Platform::current();
    let snapshot = prereq::verify(checkout, &ctx.settings, &workspaces, platform, &marker)?;

    let started = Instant::now();
    let mut run = Run::start_with_id(
        &ctx.state,
        LockKey::worktree(checkout),
        if only.is_some() {
            RunKind::CheckOnly
        } else if compile_only {
            RunKind::CheckCompile
        } else {
            RunKind::Check
        },
        "check",
        &checkout.root,
        Some(&marker),
    )?;
    let run_id = run.id().to_owned();
    *id = Some(run_id.clone());
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
        counts: None,
        compile_only,
        only,
    };
    c.hooks()?;
    if !c.stop {
        c.cargo_steps()?;
    }
    if !c.stop {
        c.project_steps()?;
    }
    c.end(&snapshot);

    let c_only = c.only.is_some();
    let Checker {
        mut run,
        extras,
        tests,
        excluded,
        counts,
        ..
    } = c;
    if let Some(mut counts) = counts {
        counts.excluded = excluded as u64;
        run.set_tests(counts);
        if run_dir.join(TEST_RESULTS).is_file() {
            run.set_test_results(TEST_RESULTS);
        }
    }
    let narrow = c_only;
    let result = run.finish(extras.is_empty())?;
    let elapsed = started.elapsed();
    if narrow {
        let passed = result.outcome == Outcome::Passed;
        println!(
            "check: only the given failures, run {run_id}: {}; log {}",
            if passed { "none appeared" } else { "FAILED" },
            run_dir.display()
        );
        return Ok(if passed { Exit::Ok } else { Exit::Fail });
    }
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
    /// The result's test counts, once a test step has run.
    counts: Option<TestCounts>,
    compile_only: bool,
    /// A narrow re-run: only these tests and steps.
    only: Option<Only>,
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
            if self.only.as_ref().is_some_and(|o| !o.steps.contains(&name)) {
                continue;
            }
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
            if self.only.as_ref().is_some_and(|o| !o.steps.contains(&name)) {
                // A narrow re-run builds what its tests need through nextest itself.
                compiled[i] = true;
                continue;
            }
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
        let mut applied_anywhere = vec![false; exclude.len()];
        let mut tried = false;
        for (i, ws) in workspaces.iter().enumerate() {
            let name = self.suffixed("tests", ws);
            let only_filter = match &self.only {
                None => None,
                Some(o) => {
                    let tests = o.tests_in(ws);
                    if tests.is_empty() {
                        continue;
                    }
                    Some(Only::filterset(&tests))
                }
            };
            if !healthy[i] {
                println!("  {name}: skipped (cargo metadata failed)");
                continue;
            }
            if !compiled[i] {
                println!("  {name}: skipped (compile failed)");
                continue;
            }
            // Only the exclusions nextest accepts in this workspace (a `package()` naming a
            // package of another workspace is an error here) go into its run.
            let mut applicable: Vec<Exclude> = Vec::new();
            let mut counts: Vec<usize> = Vec::new();
            for (k, e) in exclude.iter().enumerate() {
                if only_filter.is_some() {
                    break;
                }
                tried = true;
                let file = format!("excluded-{}-{k}.json", file_tag(ws));
                match self.list_matching(&format!("{name}:exclusion-{k}"), ws, &e.filter, &file)? {
                    Some(n) => {
                        applied_anywhere[k] = true;
                        applicable.push(e.clone());
                        counts.push(n);
                        println!("  excluded: {} ({})", e.filter, e.reason);
                    }
                    None => println!(
                        "  excluded: {} is not applied in workspace {}: nextest does not accept it there",
                        e.filter, ws.rel
                    ),
                }
                if self.stop {
                    return Ok(());
                }
            }
            if applicable.len() == 1 {
                self.excluded += counts[0];
            } else if applicable.len() > 1 {
                // Overlapping exclusions count each test once.
                let any = applicable
                    .iter()
                    .map(|e| format!("({})", e.filter))
                    .collect::<Vec<_>>()
                    .join(" | ");
                let file = format!("excluded-{}.json", file_tag(ws));
                if let Some(n) =
                    self.list_matching(&format!("{name}:exclusions"), ws, &any, &file)?
                {
                    self.excluded += n;
                }
            }
            let exclude = applicable;
            let mut argv = vec!["cargo".to_owned()];
            match &only_filter {
                None => argv.extend(nextest::run_args(&exclude)),
                Some(f) => {
                    argv.extend(["nextest", "run"].map(str::to_owned));
                    argv.extend(nextest::FLAGS.iter().map(|s| (*s).to_owned()));
                    // A listed test the tree no longer has is a failure that does not appear.
                    argv.push("--no-tests=pass".to_owned());
                    // The tests named failed in a full run, so no exclusion applies to them.
                    argv.push("-E".to_owned());
                    argv.push(f.clone());
                }
            }
            let file = format!("tests-{}.jsonl", file_tag(ws));
            let ok = self.step(&name, &argv, &ws.dir, Some(&file))?;
            let stream = fs::read_to_string(self.run.dir().join(&file)).unwrap_or_default();
            let results = nextest::parse_results(&stream);
            if let Some(o) = &self.only {
                let listed = o.tests_in(ws);
                let appeared = results.iter().any(|r| {
                    r.verdict == Verdict::Failed && listed.iter().any(|t| t.name == r.name)
                });
                if !ok && !appeared && !results.is_empty() {
                    // Only same-named tests of other binaries failed: not a listed failure.
                    self.run.discard_failed_step();
                    println!("    none of the listed tests failed");
                }
            }
            let counts = self.counts.get_or_insert_with(TestCounts::default);
            for r in &results {
                match r.verdict {
                    Verdict::Passed => counts.passed += 1,
                    Verdict::Failed => counts.failed += 1,
                    Verdict::Ignored => counts.skipped += 1,
                }
            }
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
            append(&self.run.dir().join(TEST_RESULTS), &tsv);
            if ok && results.is_empty() && self.only.is_none() {
                self.fail(
                    &name,
                    "nextest passed but reported no test results".to_owned(),
                );
            }
            if self.stop {
                return Ok(());
            }
        }
        if tried {
            for (k, e) in exclude.iter().enumerate() {
                if !applied_anywhere[k] {
                    self.fail(
                        "exclusions",
                        format!(
                            "exclusion {} ({}) is not accepted by nextest in any workspace; nothing was left out for it",
                            e.filter, e.reason
                        ),
                    );
                }
            }
        }
        Ok(())
    }

    /// `cargo nextest list` with `filter` in `ws`: how many tests it matches, or `None` when
    /// nextest rejects it there. A rejection is an answer, not a failure of the run.
    fn list_matching(
        &mut self,
        name: &str,
        ws: &Workspace,
        filter: &str,
        file: &str,
    ) -> Result<Option<usize>> {
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
        argv.push(filter.to_owned());
        let env = Self::env(nextest::JSON_ENV);
        let rec = self
            .run
            .step_with_stdout(name, &argv, &ws.dir, &env, Some(file))?;
        if self.run.interrupted().is_some() {
            self.stop = true;
        }
        if rec.status == StepStatus::Failed {
            self.run.discard_failed_step();
            return Ok(None);
        }
        if !rec.passed() {
            println!("  {name}: could not list the excluded tests ({})", rec.log);
            return Ok(None);
        }
        let text = fs::read_to_string(self.run.dir().join(file)).unwrap_or_default();
        Ok(Some(nextest::count_matching(&text)))
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
            .filter(|s| self.only.as_ref().is_none_or(|o| o.steps.contains(&s.name)))
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
        for line in prereq::recheck_nextest(self.checkout(), snapshot) {
            self.fail("nextest-config", line);
        }
        let marker = self.run.id().to_owned();
        let lines = prereq::recheck(
            self.checkout(),
            &self.ctx.settings,
            self.workspaces,
            snapshot,
            &marker,
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
