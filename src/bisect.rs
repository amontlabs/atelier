//! Red batch bisection (DESIGN §8.3). When a gate over more than one branch is RED, the RED
//! verdict stands: nothing is rerun on that tree. To decide what can still land, prefixes of the
//! batch are merged in merge order, longest first, and on each only what failed is run: the
//! failed tests by name (`atelier check --only`), a failed step whole, a platform whose failure
//! cannot be narrowed whole. A prefix whose merged tree was already judged in this search (the
//! failed batch's included) takes that tree's result and is not run again. The first prefix on
//! which none of the original failures appear is the longest green one; the branch right after
//! it is the culprit. The search is linear because failures are not monotone across prefixes.
//!
//! The caller then sends the green prefix through one full gate as a smaller batch; the culprit
//! and the branches after it stay approved.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::check::{Only, OnlyTest};
use crate::exit::{Failure, Result};
use crate::gate::{self, Item, Merged, PlatformOutcome};
use crate::git;
use crate::locks::HeldLock;
use crate::preflight::Context;
use crate::runs::{Run, RunKind, RunResult, StepStatus};
use crate::settings::Settings;
use crate::stack::short;

/// The run kind of a bisection (kept in `run.toml` as written: a kind later versions may name).
pub const KIND: &str = "gate-bisect";

/// What failed on one platform of a RED gate, as bisection re-runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failures {
    pub platform: String,
    /// The failure cannot be narrowed: the platform's whole run is repeated.
    pub whole: bool,
    /// The failed tests by name and the failed steps.
    pub only: Only,
}

impl Failures {
    /// A failure that is re-run whole.
    pub fn whole(platform: &str) -> Failures {
        Failures {
            platform: platform.to_owned(),
            whole: true,
            only: Only::default(),
        }
    }

    /// The failures of an `atelier check` result: its failed tests (`tests.tsv`) and failed
    /// steps. A failed `check.prepare` hook, a test step that failed without a failed test, and
    /// a failure that is no step's (the tree, the build folder) make it whole.
    pub fn from_check(platform: &str, run_dir: &Path, r: &RunResult) -> Failures {
        let mut only = Only::default();
        if let Ok(text) = fs::read_to_string(run_dir.join("tests.tsv")) {
            for line in text.lines() {
                let f: Vec<&str> = line.split('\t').collect();
                if f.len() >= 3 && f[2] == "fail" {
                    only.tests.push(OnlyTest {
                        workspace: f[0].to_owned(),
                        name: f[1].to_owned(),
                    });
                }
            }
        }
        let mut whole = false;
        for s in r.steps.iter().filter(|s| s.status != StepStatus::Passed) {
            let n = &s.name;
            if n.starts_with("prepare:") || n.contains(":exclusion") {
                whole = true;
            } else if n == "tests" || n.starts_with("tests@") {
                let ws = n.strip_prefix("tests@").unwrap_or(".");
                let named = only.tests.iter().any(|t| n == "tests" || t.workspace == ws);
                if !named || s.status != StepStatus::Failed {
                    whole = true;
                }
            } else if s.status == StepStatus::Failed {
                only.steps.push(n.clone());
            } else {
                // Left processes behind, interrupted, not started: not narrowed.
                whole = true;
            }
        }
        let step_failures = r
            .steps
            .iter()
            .filter(|s| s.status != StepStatus::Passed)
            .count();
        if r.failures.len() > step_failures || !r.survivors.is_empty() {
            whole = true;
        }
        if only.tests.is_empty() && only.steps.is_empty() {
            whole = true;
        }
        Failures {
            platform: platform.to_owned(),
            whole,
            only,
        }
    }

    /// `tests a, b; steps links` or `its whole run`, for messages.
    pub fn describe(&self) -> String {
        if self.whole {
            return format!("{}: its whole run", self.platform);
        }
        let mut parts = Vec::new();
        if !self.only.tests.is_empty() {
            parts.push(
                self.only
                    .tests
                    .iter()
                    .map(|t| t.name.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        if !self.only.steps.is_empty() {
            parts.push(format!("step {}", self.only.steps.join(", ")));
        }
        format!("{}: {}", self.platform, parts.join("; "))
    }
}

/// One bisection after a RED gate, under the gate lock.
pub struct Search<'a> {
    pub ctx: &'a Context,
    pub items: &'a [Item],
    pub old: &'a str,
    /// The failed batch's merged commit.
    pub red_m: &'a str,
    pub red_run: &'a str,
    pub failures: &'a [Failures],
}

impl Search<'_> {
    /// Runs the search; returns the lock and the length of the longest green prefix (0: even
    /// the first branch alone fails, nothing lands).
    pub fn run(&self, lock: HeldLock) -> Result<(HeldLock, usize)> {
        let root = &self.ctx.checkout.root;
        let n = self.items.len();
        let names: Vec<&str> = self.items.iter().map(|i| i.name.as_str()).collect();
        let what = describe_all(self.failures);
        println!(
            "merge: finding what breaks the batch ({}): prefixes longest first, re-running only {what}",
            names.join(", ")
        );
        let gate_path = gate::gate_worktree(&self.ctx.checkout);
        let gate = match gate::prepare_gate(root, &gate_path, self.old) {
            Ok(g) => g,
            Err(e) => {
                println!(
                    "merge: no search: {}; the RED verdict stands and every branch stays approved",
                    e.message
                );
                return Ok((lock, 0));
            }
        };
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| Failure::refused("HOME is not set"))?;
        let mut run = Run::start_held(
            &self.ctx.state,
            lock,
            RunKind::Other(KIND.to_owned()),
            &format!("bisect after run {}", self.red_run),
            &gate.root,
        )?;
        run.set_branch(Some(names.join(" ")));
        let runs_dir = self.ctx.state.runs_dir();
        let regen = gate::regenerated_paths(&self.ctx.settings);

        // Tree → verdict (true: none of the failures appeared).
        let mut judged: BTreeMap<String, (bool, String)> = BTreeMap::new();
        let red_tree = git::run(
            &gate.root,
            ["rev-parse", &format!("{}^{{tree}}", self.red_m)],
        )?;
        judged.insert(red_tree, (false, "the failed batch".to_owned()));

        let mut green = 0;
        let mut stopped = None;
        'prefixes: for k in (1..n).rev() {
            let label = names[..k].join(", ");
            if let Err(e) = gate::checkout_detached(&gate.root, self.old) {
                stopped = Some(e.message);
                break;
            }
            for item in &self.items[..k] {
                match gate::merge_one(&gate.root, item, &regen) {
                    Ok(Merged::Clean) | Ok(Merged::Settled(_)) => {}
                    Ok(Merged::Conflict(files)) => {
                        stopped = Some(format!(
                            "prefix {label}: {} conflicts ({})",
                            item.name,
                            files.join(", ")
                        ));
                        break 'prefixes;
                    }
                    Err(e) => {
                        stopped = Some(e.message);
                        break 'prefixes;
                    }
                }
            }
            let settings = match Settings::load(&gate.root) {
                Ok(s) => s,
                Err(e) => {
                    stopped = Some(e.message);
                    break;
                }
            };
            let hooks_ok = gate::after_hooks(&mut run, &gate.root, &settings)?;
            let tree = git::run(&gate.root, ["rev-parse", "HEAD^{tree}"])?;
            let clean = gate::unclean_paths(&gate.root)?.is_empty();
            let verdict = if !hooks_ok || !clean {
                println!("  prefix {label}: its merge.after hooks failed");
                false
            } else if let Some((v, of)) = judged.get(&tree) {
                println!(
                    "  prefix {label}: the same tree as {of}, already judged: {}",
                    word(*v)
                );
                *v
            } else {
                let mut all = true;
                for f in self.failures {
                    if !gate::is_local(&f.platform) {
                        stopped = Some(format!(
                            "{} ran on another machine: re-running it comes with module 3.6",
                            f.platform
                        ));
                        break 'prefixes;
                    }
                    let only = (!f.whole).then_some(&f.only);
                    let out = gate::run_local_in(
                        &mut run,
                        &runs_dir,
                        &gate.root,
                        &settings,
                        &home,
                        &f.platform,
                        only,
                    )?;
                    if !matches!(out, PlatformOutcome::Passed { .. }) {
                        all = false;
                    }
                    if run.interrupted().is_some() {
                        stopped = Some("interrupted".to_owned());
                        break 'prefixes;
                    }
                }
                println!("  prefix {label}: {}", word(all));
                judged.insert(tree, (all, format!("prefix {label}")));
                all
            };
            if verdict {
                green = k;
                break;
            }
        }

        let failures = describe_all(self.failures);
        let (passed, lock) = if let Some(why) = stopped {
            run.fail(format!("search stopped: {why}"));
            println!(
                "merge: the search stopped ({why}); the RED verdict stands and every branch stays approved"
            );
            let (_, lock) = run.finish_keeping_lock(false)?;
            green = 0;
            (false, lock)
        } else {
            if green == n - 1 {
                println!(
                    "merge: fails only with the whole batch ({}), or is flaky: {failures}",
                    names[n - 1]
                );
            } else {
                println!("merge: {} breaks: {failures}", names[green]);
            }
            if green == 0 {
                println!("merge: nothing lands; {} stay approved", names.join(", "));
            }
            let (_, lock) = run.finish_keeping_lock(true)?;
            (true, lock)
        };
        let _ = passed;
        if green == 0 {
            // Back to the failed batch's tree, for inspection.
            let _ = gate::checkout_detached(&gate.root, self.red_m);
        }
        let _ = short(root, self.old);
        Ok((lock, green))
    }
}

fn word(v: bool) -> &'static str {
    if v {
        "none of the failures appear"
    } else {
        "fails"
    }
}

fn describe_all(failures: &[Failures]) -> String {
    if failures.is_empty() {
        return "the merge.after hooks".to_owned();
    }
    failures
        .iter()
        .map(Failures::describe)
        .collect::<Vec<_>>()
        .join("; ")
}
