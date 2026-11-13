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
use std::path::Path;

use crate::check::{Only, OnlyTest};
use crate::exit::Result;
use crate::gate::{self, GateRun, Item, Merged, PlatformOutcome, Platforms};
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
    /// The machine it failed on, when not this computer: the re-run goes there first.
    pub machine: Option<String>,
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
            machine: None,
            whole: true,
            only: Only::default(),
        }
    }

    /// A failure on another machine, re-run whole there: its failed tests are on that machine.
    pub fn whole_on(platform: &str, machine: &str) -> Failures {
        Failures {
            machine: Some(machine.to_owned()),
            ..Self::whole(platform)
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
            machine: None,
            whole,
            only,
        }
    }

    /// `tests a, b; steps links` or `its whole run`, for messages.
    pub fn describe(&self) -> String {
        if self.whole {
            return match &self.machine {
                Some(m) => format!("{}: its whole run (on {m})", self.platform),
                None => format!("{}: its whole run", self.platform),
            };
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
    /// OLD's committed settings: which conflicts may be settled (HF1).
    pub old_settings: &'a Settings,
    /// The failed batch's merged commit.
    pub red_m: &'a str,
    pub red_run: &'a str,
    pub failures: &'a [Failures],
    /// What made the gate RED, for a RED that is no platform's.
    pub cause: &'a str,
}

impl Search<'_> {
    /// Runs the search; returns the lock and the length of the longest green prefix (0: even
    /// the first branch alone fails, nothing lands).
    pub fn run(&self, lock: HeldLock, platforms: &mut dyn Platforms) -> Result<(HeldLock, usize)> {
        let root = &self.ctx.checkout.root;
        let n = self.items.len();
        let names: Vec<&str> = self.items.iter().map(|i| i.name.as_str()).collect();
        if self.failures.is_empty() {
            // The merged settings, the hooks or the tree made it RED: no platform failure to
            // re-run, so no prefix can be judged by one.
            println!(
                "merge: no search: the RED is not a platform's failure ({}); re-running prefixes cannot tell which branch causes it, so no branch is blamed; {} stay approved",
                self.cause,
                names.join(", ")
            );
            return Ok((lock, 0));
        }
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
        let mut run = Run::start_held(
            &self.ctx.state,
            lock,
            RunKind::Other(KIND.to_owned()),
            &format!("bisect after run {}", self.red_run),
            &gate.root,
        )?;
        run.set_branch(Some(names.join(" ")));
        let old_regen = gate::regenerated_paths(self.old_settings);

        // Tree → (verdict, what judged it, what failed on it).
        let mut judged: BTreeMap<String, (bool, String, String)> = BTreeMap::new();
        let red_tree = git::run(
            &gate.root,
            ["rev-parse", &format!("{}^{{tree}}", self.red_m)],
        )?;
        judged.insert(
            red_tree,
            (false, "the failed batch".to_owned(), what.clone()),
        );

        // What failed on each prefix length judged (n: the failed batch).
        let mut failed_on: BTreeMap<usize, String> = BTreeMap::new();
        failed_on.insert(n, what.clone());
        let mut green = 0;
        let mut stopped = None;
        'prefixes: for k in (1..n).rev() {
            let label = names[..k].join(", ");
            if let Err(e) = gate::checkout_detached(&gate.root, self.old) {
                stopped = Some(e.message);
                break;
            }
            let mut settled = std::collections::BTreeSet::new();
            for item in &self.items[..k] {
                match gate::merge_one(&gate.root, item, &old_regen) {
                    Ok(Merged::Clean) => {}
                    Ok(Merged::Settled(paths)) => settled.extend(paths),
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
                    stopped = Some(format!("prefix {label}: {}", e.message));
                    break;
                }
            };
            let hooks = gate::after_hooks(&mut run, &gate.root, &settings)?;
            let m = git::run(&gate.root, ["rev-parse", "HEAD"])?;
            let mut hooks_ok = hooks.ok;
            if hooks_ok {
                hooks_ok =
                    gate::verify_settled(&mut run, &gate, &settings, &m, &settled, &hooks.touched)?;
            }
            // Leftovers in the gate would come into every shorter prefix; atelier deletes
            // nothing there, so the search stops.
            let dirty = gate::unclean_paths(&gate.root)?;
            if !dirty.is_empty() {
                stopped = Some(format!(
                    "prefix {label}: the gate is not clean after its merge.after hooks: {}; look at each path in {}, keep what matters and remove the rest",
                    dirty.join(", "),
                    gate.root.display()
                ));
                break;
            }
            if run.interrupted().is_some() {
                stopped = Some("interrupted".to_owned());
                break;
            }
            let tree = git::run(&gate.root, ["rev-parse", "HEAD^{tree}"])?;
            let (verdict, failed) = if !hooks_ok {
                let why = format!(
                    "its merge.after hooks: {}",
                    run.failures().last().cloned().unwrap_or_default()
                );
                println!("  prefix {label}: {why}");
                (false, why)
            } else if let Some((v, of, failed)) = judged.get(&tree) {
                println!(
                    "  prefix {label}: the same tree as {of}, already judged: {}",
                    word(*v)
                );
                (*v, failed.clone())
            } else {
                let names_needed: Vec<String> =
                    self.failures.iter().map(|f| f.platform.clone()).collect();
                if let Err(e) = gate::pinned_installed(&settings, &names_needed, &m, root) {
                    stopped = Some(format!("prefix {label}: {}", e.message));
                    break;
                }
                let mut all = true;
                let mut what_failed = Vec::new();
                for f in self.failures {
                    let out = {
                        let mut g = GateRun {
                            ctx: self.ctx,
                            run: &mut run,
                            gate: &gate,
                            settings: &settings,
                            m: &m,
                        };
                        platforms.rerun(&mut g, f)?
                    };
                    match out {
                        PlatformOutcome::Passed { .. } => {}
                        PlatformOutcome::Failed { what, failures, .. } => {
                            all = false;
                            what_failed.push(if failures.whole {
                                format!("{}: {what}", f.platform)
                            } else {
                                format!("{} ({what})", failures.describe())
                            });
                        }
                        PlatformOutcome::NoMachine(why) => {
                            stopped = Some(format!("prefix {label}: {why}"));
                            break 'prefixes;
                        }
                    }
                    if run.interrupted().is_some() {
                        stopped = Some("interrupted".to_owned());
                        break 'prefixes;
                    }
                }
                println!("  prefix {label}: {}", word(all));
                let failed = what_failed.join("; ");
                judged.insert(tree, (all, format!("prefix {label}"), failed.clone()));
                (all, failed)
            };
            if verdict {
                green = k;
                break;
            }
            failed_on.insert(k, failed);
        }

        let lock = if let Some(why) = stopped {
            run.fail(format!("search stopped: {why}"));
            println!(
                "merge: the search stopped ({why}); the RED verdict stands and every branch stays approved"
            );
            let (_, lock) = run.finish_keeping_lock(false)?;
            green = 0;
            lock
        } else {
            let culprit = names[green];
            let failed = failed_on.get(&(green + 1)).cloned().unwrap_or_default();
            if green == n - 1 {
                println!(
                    "merge: fails only with the whole batch ({culprit}), or is flaky: {failed}"
                );
            } else {
                println!(
                    "merge: {culprit} breaks: on prefix {}: {failed}",
                    names[..=green].join(", ")
                );
            }
            if green == 0 {
                println!("merge: nothing lands; {} stay approved", names.join(", "));
            }
            let (_, lock) = run.finish_keeping_lock(true)?;
            lock
        };
        if green == 0 {
            // Back to the failed batch's tree, for inspection, when the gate allows it.
            if gate::unclean_paths(&gate.root)
                .map(|d| d.is_empty())
                .unwrap_or(false)
            {
                let _ = gate::checkout_detached(&gate.root, self.red_m);
            }
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
    failures
        .iter()
        .map(Failures::describe)
        .collect::<Vec<_>>()
        .join("; ")
}
