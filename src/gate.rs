//! `atelier merge <branch>… [--waive <platform> --reason "<owner's words>"]` (DESIGN §3 `merge`,
//! r4 §3.8): merges approved branches in the gate worktree, checks the merged tree on every
//! platform, and moves the integration branch only when that tree is green and nobody moved the
//! branch meanwhile (mode A, `promote`).
//!
//! 1. Under the gate lock: a pending promotion is settled first (the three questions) or
//!    refused; mode A's checks a and b; each branch approved, still at its approved commit,
//!    current, with no merge commit of its own; a lower branch lands only when it is in the
//!    batch, earlier, approved at a commit that contains the upper branch's base (DESIGN §2.4);
//!    a branch that changes `.atelier/` or the generated rules merges alone; every platform of
//!    the settings the batch produces ([`batch_settings`]) has a machine that can run their pin,
//!    or is waived.
//! 2. The gate worktree `<main checkout>.gate` must be clean; atelier deletes nothing there.
//!    It is checked out at `OLD`, the integration branch's commit.
//! 3. Each approved commit is merged with `--no-ff`, in order, with no global or system
//!    attributes; a merge driver other than git's own for a path both sides changed refuses.
//!    A conflict only in paths a `merge.after` hook with `commit` regenerates, in OLD's committed
//!    settings and in the merged ones, is settled by taking the incoming branch's side for those
//!    paths (HF1), never for a rules item; any other conflict aborts, returns the gate to `OLD`
//!    and exits 5 naming what the branch conflicts with.
//! 4. The `merge.after` hooks; a hook with `commit` has its changes committed, any other change
//!    is RED. A settled path must then hold what its hook produces. The result is `M`.
//! 5. Every platform's `run` for `M`, from the merged settings, all at the same time, on the
//!    machines [`crate::gate_machines`] chooses. A `run = "check"` platform here runs
//!    `atelier check` through `M`'s own pinned atelier.
//! 6. RED: exit 1, the integration branch unmoved, the gate kept for inspection; a batch of
//!    more than one branch is then searched for the branch that breaks it (`bisect`).
//! 7. Green: the promotion record, then the push (`promote`), then one `merges.tsv` line per
//!    branch and its record marked merged with the merge facts `clean` trusts.

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::bisect;
use crate::check::duration;
use crate::dispatch::{self, DISPATCHED_VAR};
use crate::exit::{Exit, Failure, Result};
use crate::git::{self, Checkout};
use crate::ledger::{Approval, PlatformRecord, PromotedBranch, Promotion};
use crate::locks::{HeldLock, LockKey};
use crate::names;
use crate::platform::Platform;
use crate::preflight::{CONVERTERS, Context};
use crate::promote::{self, Caller};
use crate::runs::{Outcome, Run, RunInfo, RunKind, RunResult, StepStatus};
use crate::settings::{PlatformRun, Settings};
use crate::stack::{Stack, is_ancestor, short};
use crate::state::CURRENT_FORMAT;
use crate::tree;

/// The main checkout's folder, canonical, from any worktree of the clone.
pub fn main_root(checkout: &Checkout) -> PathBuf {
    let common = git::canonical(&checkout.common_dir);
    common.parent().map(Path::to_path_buf).unwrap_or(common)
}

/// The one gate worktree of a clone: `<main checkout>.gate` (DESIGN §2.2, r4 §3.8 step 2).
/// `approve`, `merge`, `tmp` and `clean` all name it through this function.
pub fn gate_worktree(checkout: &Checkout) -> PathBuf {
    let main = main_root(checkout);
    let mut name = main.file_name().map(|n| n.to_owned()).unwrap_or_default();
    name.push(".gate");
    main.with_file_name(name)
}

/// The owner's waiver of one platform for this merge.
#[derive(Debug, Clone)]
pub struct Waiver {
    pub platform: String,
    pub reason: String,
}

/// One approved branch of the batch, in merge order.
#[derive(Debug, Clone)]
pub struct Item {
    pub name: String,
    pub lifetime: String,
    /// The approved commit, which is merged.
    pub commit: String,
    pub approval: Approval,
    /// It changes `.atelier/` or the generated rules.
    pub rules: bool,
}

/// What a platform's run for `M` gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformOutcome {
    Passed {
        machine: Option<String>,
        run: Option<String>,
    },
    Failed {
        machine: Option<String>,
        run: Option<String>,
        /// What failed, for the RED line.
        what: String,
        /// The failed tests by name and failed steps, for bisection.
        failures: bisect::Failures,
    },
    /// No machine could take it: the reasons, per machine.
    NoMachine(String),
    /// Stopped before its verdict because another platform failed on this computer, which
    /// already makes the gate RED: what was running, and where.
    Cancelled(String),
}

/// Where a platform can run, as checked before anything runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// This computer, in the gate worktree.
    Local,
    /// The named machine answered: ready, or busy and waited for.
    Machine(String),
    /// No machine is available; the reason.
    None(String),
}

/// Runs platforms for the gate: [`crate::gate_machines::Machines`], the owner's machines file
/// and remote runs (module 3.6).
pub trait Platforms {
    /// Whether some machine can take `platform` of `settings`, in seconds (step 1, and again
    /// for a platform the merged settings add or change).
    fn availability(
        &self,
        checkout: &Checkout,
        settings: &Settings,
        platform: &str,
    ) -> Availability;
    /// Runs each of `platforms`' `run` for the merged commit checked out in the gate, all at the
    /// same time (step 5). One outcome per platform, in the order given.
    fn run_all(
        &mut self,
        g: &mut GateRun<'_>,
        platforms: &[String],
    ) -> Result<Vec<(String, PlatformOutcome)>>;
    /// Bisection: re-runs one platform's failures for the prefix checked out in the gate, on
    /// the machine they failed on first.
    fn rerun(
        &mut self,
        g: &mut GateRun<'_>,
        failures: &bisect::Failures,
    ) -> Result<PlatformOutcome>;
}

pub(crate) fn is_local(platform: &str) -> bool {
    Platform::parse(platform).is_some() && Platform::current() == Platform::parse(platform)
}

/// The gate's run and what platform runs need from it.
pub struct GateRun<'a> {
    pub ctx: &'a Context,
    pub run: &'a mut Run,
    /// The gate worktree.
    pub gate: &'a Checkout,
    /// The merged commit's settings.
    pub settings: &'a Settings,
    /// The merged commit.
    pub m: &'a str,
}

/// `$HOME`, where the pinned ateliers and the machines file are.
pub fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Failure::refused("HOME is not set"))
}

/// The settings committed at `spec` (`<commit>:<path>`, or `:<path>` for the index), parsed;
/// `None` when there is no such file. Never the main checkout's working file.
pub fn settings_at(dir: &Path, commit: &str) -> Result<Option<Settings>> {
    let spec = format!("{commit}:{}", crate::settings::SETTINGS_PATH);
    let out = git::output(dir, ["show", &spec])?;
    if !out.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Settings::parse(&text)
        .map(Some)
        .map_err(|e| Failure::refused(format!("{spec}: {}", e.message)))
}

/// The waiver of `platform`, if the owner gave one.
pub fn waived<'w>(waivers: &'w [Waiver], platform: &str) -> Option<&'w Waiver> {
    waivers.iter().find(|w| w.platform == platform)
}

/// `atelier merge`.
pub fn run(ctx: &Context, branches: &[String], waivers: &[Waiver]) -> Result<Exit> {
    let mut platforms = crate::gate_machines::Machines::load(&home()?)?;
    run_with(ctx, branches, waivers, &mut platforms)
}

/// `atelier merge` with the given platform runner.
pub fn run_with(
    ctx: &Context,
    branches: &[String],
    waivers: &[Waiver],
    platforms: &mut dyn Platforms,
) -> Result<Exit> {
    let started = Instant::now();
    let root = ctx.checkout.root.clone();
    let integration = ctx.settings.integration_branch.clone();
    let mut seen = BTreeSet::new();
    for b in branches {
        names::validate_branch(&root, b)?;
        if !seen.insert(b.as_str()) {
            return Err(Failure::usage(format!("{b} is named twice")));
        }
        if *b == integration {
            return Err(Failure::usage(format!(
                "{b} is the integration branch: merge the branches built on it"
            )));
        }
    }
    let mut waived_seen = BTreeSet::new();
    for w in waivers {
        if !waived_seen.insert(w.platform.as_str()) {
            return Err(Failure::usage(format!(
                "--waive {}: named twice",
                w.platform
            )));
        }
        if w.reason.trim().is_empty() {
            return Err(Failure::usage("--reason: the owner's words are required"));
        }
    }

    let v = ctx.state.versioned(CURRENT_FORMAT, CONVERTERS)?;
    let lock = HeldLock::acquire(&ctx.state, LockKey::Gate)?;
    if let Some(rec) = Promotion::read(&v)? {
        let log_dir = ctx.state.runs_dir().join(&rec.run);
        promote::settle_found(ctx, &v, &rec, &log_dir)?;
    }
    promote::check_ab(&root, &integration)?;

    let stack = Stack::new(&ctx.checkout, &ctx.state, &integration);
    let old = stack.integration_tip()?;
    let old_settings = settings_at(&root, &old)?.ok_or_else(|| {
        Failure::refused(format!(
            "{integration} ({}) has no {}",
            short(&root, &old),
            crate::settings::SETTINGS_PATH
        ))
    })?;
    let items = preconditions(ctx, &v, &stack, branches, &old_settings)?;

    // The settings the batch produces decide the platforms and the pin from the start, so a
    // missing machine costs seconds, not a whole run. A waiver names a platform of OLD's
    // settings or of the merged ones.
    let merged = batch_settings(&root, &items, &old_settings)?;
    for w in waivers {
        if !old_settings.platforms.contains_key(&w.platform)
            && !merged.platforms.contains_key(&w.platform)
        {
            return Err(Failure::refused(format!(
                "--waive {}: not a platform of this project (it has: {})",
                w.platform,
                platform_names(&merged).join(", ")
            )));
        }
    }

    // The forecast only warns.
    let open = crate::forecast::open_branches(&stack);
    let main = crate::forecast::Open {
        name: integration.clone(),
        tip: old.clone(),
    };
    let forecast = crate::forecast::forecast(
        &ctx.checkout,
        &main,
        &open,
        &regenerated_paths(&old_settings),
    );
    let batch: Vec<String> = items.iter().map(|i| i.name.clone()).collect();
    for line in crate::forecast::warning_lines(&forecast, &batch) {
        println!("{line}");
    }

    let names: Vec<String> = platform_names(&merged)
        .into_iter()
        .filter(|p| waived(waivers, p).is_none())
        .collect();
    require_machines(platforms, &ctx.checkout, &merged, &names, &integration)?;

    let end = gate(
        ctx,
        &v,
        lock,
        &items,
        &old,
        &old_settings,
        waivers,
        platforms,
        started,
    )?;
    match end {
        GateEnd::Done(exit) => Ok(exit),
        GateEnd::Green {
            rec,
            mut lock,
            run_dir,
        } => promote::promote(
            ctx,
            &v,
            &mut lock,
            *rec,
            &Caller {
                word: "merge",
                log_dir: run_dir,
            },
        ),
        GateEnd::Red {
            lock,
            failures,
            run_id,
            m,
            cause,
        } => {
            if items.len() < 2 {
                return Ok(Exit::Fail);
            }
            let search = bisect::Search {
                ctx,
                items: &items,
                old: &old,
                old_settings: &old_settings,
                red_m: &m,
                red_run: &run_id,
                failures: &failures,
                cause: &cause,
            };
            let (lock, green) = search.run(lock, platforms)?;
            if green == 0 {
                return Ok(Exit::Fail);
            }
            let prefix = &items[..green];
            println!(
                "merge: the green prefix ({}) goes through one full gate",
                prefix
                    .iter()
                    .map(|i| i.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            match gate(
                ctx,
                &v,
                lock,
                prefix,
                &old,
                &old_settings,
                waivers,
                platforms,
                Instant::now(),
            )? {
                GateEnd::Green {
                    rec,
                    mut lock,
                    run_dir,
                } => {
                    let exit = promote::promote(
                        ctx,
                        &v,
                        &mut lock,
                        *rec,
                        &Caller {
                            word: "merge",
                            log_dir: run_dir,
                        },
                    )?;
                    let rest: Vec<&str> = items[green..].iter().map(|i| i.name.as_str()).collect();
                    println!("still approved, not merged: {}", rest.join(", "));
                    Ok(if exit == Exit::Ok { Exit::Fail } else { exit })
                }
                GateEnd::Red { .. } => Ok(Exit::Fail),
                GateEnd::Done(exit) => Ok(exit),
            }
        }
    }
}

/// Exit 3: the platforms without a machine.
fn no_machine(integration: &str, missing: &[String]) -> Failure {
    Failure::new(
        Exit::NoMachine,
        format!(
            "no machine for every platform; nothing ran and {integration} has not moved:\n  {}\nwaive a platform with --waive <platform> --reason \"<the owner's words>\"",
            missing.join("\n  ")
        ),
    )
}

/// The settings the batch produces, read before anything merges: a rules item merges alone, so
/// its approved commit's committed settings are the merged ones; any other batch keeps OLD's.
pub fn batch_settings(root: &Path, items: &[Item], old_settings: &Settings) -> Result<Settings> {
    match items.iter().find(|i| i.rules) {
        Some(i) => Ok(settings_at(root, &i.commit)?.unwrap_or_else(|| old_settings.clone())),
        None => Ok(old_settings.clone()),
    }
}

/// Every one of `names` has a machine for `settings` (exit 3, listing why not), and one that
/// this computer takes can run `settings`' pin there (refused, 4: [`pin_missing_here`]).
fn require_machines(
    platforms: &dyn Platforms,
    checkout: &Checkout,
    settings: &Settings,
    names: &[String],
    integration: &str,
) -> Result<()> {
    let mut missing = Vec::new();
    let mut here = Vec::new();
    for p in names {
        match platforms.availability(checkout, settings, p) {
            Availability::None(why) => missing.push(why),
            Availability::Local => here.push(p.clone()),
            Availability::Machine(_) => {}
        }
    }
    if !missing.is_empty() {
        return Err(no_machine(integration, &missing));
    }
    let home = home()?;
    for p in here {
        if let Some(why) = pin_missing_here(settings, &p, &home) {
            return Err(Failure::refused(format!("{p}: {why}")));
        }
    }
    Ok(())
}

/// Why this computer cannot run `platform` of `settings`, when it runs `atelier check` and the
/// atelier those settings pin is not installed here: the check runs through that atelier.
/// Another machine is asked the same at its probe ([`crate::remote::Remote::pinned`]).
pub fn pin_missing_here(settings: &Settings, platform: &str, home: &Path) -> Option<String> {
    let check = settings
        .platforms
        .get(platform)
        .is_some_and(|ps| matches!(ps.run, PlatformRun::Named(_)));
    let pin = &settings.atelier;
    (check && !dispatch::binary_for(home, pin).is_file()).then(|| {
        format!(
            "atelier {pin}, pinned by the merged commit, is not installed on this computer, so its check cannot run here: {}",
            dispatch::install_command(pin)
        )
    })
}

/// How one gate over a batch ended.
pub enum GateEnd {
    /// Green: the promotion record is written; the gate lock is held for the promotion.
    Green {
        rec: Box<Promotion>,
        lock: HeldLock,
        run_dir: PathBuf,
    },
    /// RED: the verdict is printed; the gate lock is held for bisection.
    Red {
        lock: HeldLock,
        /// What failed per platform; empty when the RED is no platform's (the merged settings,
        /// the hooks, the tree).
        failures: Vec<bisect::Failures>,
        run_id: String,
        m: String,
        /// The run's failure lines: what made it RED.
        cause: String,
    },
    /// Ended otherwise (interrupted); the lock is released.
    Done(Exit),
}

/// The paths OLD's committed settings name in `regenerates` of a hook with `commit`: the only
/// conflicts the gate may settle (HF1), and only when the merged settings name them too.
pub fn regenerated_paths(settings: &Settings) -> BTreeSet<String> {
    settings
        .merge
        .after
        .iter()
        .filter(|h| h.commit.is_some())
        .flat_map(|h| h.regenerates.iter().cloned())
        .collect()
}

/// Ends the gate's run as failed and returns `e`.
fn refuse_in(mut run: Run, e: Failure) -> Result<GateEnd> {
    run.fail(e.message.clone());
    let _ = run.finish(false);
    Err(e)
}

/// Steps 2–6 for a batch whose preconditions passed, under the gate lock. A conflict (5), a
/// missing machine (3) and refusals (4) are errors; the lock is released with them.
#[allow(clippy::too_many_arguments)]
pub fn gate(
    ctx: &Context,
    v: &crate::state::Versioned,
    lock: HeldLock,
    items: &[Item],
    old: &str,
    old_settings: &Settings,
    waivers: &[Waiver],
    platforms: &mut dyn Platforms,
    started: Instant,
) -> Result<GateEnd> {
    let root = ctx.checkout.root.clone();
    let integration = ctx.settings.integration_branch.clone();
    let gate_path = gate_worktree(&ctx.checkout);
    let gate = prepare_gate(&root, &gate_path, old)?;
    let names_line = items
        .iter()
        .map(|i| i.name.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let mut run = Run::start_held(
        &ctx.state,
        lock,
        RunKind::Gate,
        &format!("merge {names_line}"),
        &gate.root,
    )?;
    run.set_branch(Some(names_line.clone()));
    let run_id = run.id().to_owned();
    let run_dir = run.dir().to_path_buf();
    println!(
        "merge: run {run_id}, {integration} {} in {}",
        short(&root, old),
        gate.root.display()
    );

    // Step 3.
    let old_regen = regenerated_paths(old_settings);
    let mut settled = BTreeSet::new();
    for (i, item) in items.iter().enumerate() {
        match merge_one(&gate.root, item, &old_regen) {
            Ok(Merged::Clean) => {
                println!(
                    "  in the gate: {} {}",
                    item.name,
                    short(&root, &item.commit)
                )
            }
            Ok(Merged::Settled(paths)) => {
                println!(
                    "  in the gate: {} {}",
                    item.name,
                    short(&root, &item.commit)
                );
                for p in paths {
                    println!(
                        "    conflict in {p} settled with {}'s side: a merge.after hook regenerates it",
                        item.name
                    );
                    settled.insert(p);
                }
            }
            Ok(Merged::Conflict(files)) => {
                let back = checkout_detached(&gate.root, old);
                let with =
                    conflict_with(&gate.root, old, &items[..i], item, &integration, &old_regen);
                let mut msg = format!("conflict: {} with {with}", item.name);
                for f in &files {
                    msg.push_str(&format!("\n  {f}"));
                }
                msg.push_str(&format!(
                    "\nresolve it in {}'s worktree (atelier follow, or merge {integration} there), then approve it again; {integration} has not moved",
                    item.name
                ));
                if let Err(e) = back {
                    msg.push_str(&format!(
                        "\nthe gate could not be returned to {}: {}",
                        short(&root, old),
                        e.message
                    ));
                }
                run.fail(format!("conflict: {} with {with}", item.name));
                let _ = run.finish(false);
                return Err(Failure::new(Exit::Conflict, msg));
            }
            Err(e) => {
                let _ = checkout_detached(&gate.root, old);
                return refuse_in(run, e);
            }
        }
        if run.interrupted().is_some() {
            return interrupted(run);
        }
    }

    // Step 4.
    let settings = match Settings::load(&gate.root) {
        Ok(s) => s,
        Err(e) => {
            run.fail(format!("the merged settings are not valid: {}", e.message));
            let m = git::run(&gate.root, ["rev-parse", "HEAD"])?;
            return red(
                run,
                &integration,
                &gate.root,
                &run_dir,
                started,
                &[],
                Vec::new(),
                m,
            );
        }
    };
    let hooks = after_hooks(&mut run, &gate.root, &settings)?;
    if run.interrupted().is_some() {
        return interrupted(run);
    }
    let m = git::run(&gate.root, ["rev-parse", "HEAD"])?;
    let mut ok = hooks.ok;
    if ok {
        ok = tree_is(&mut run, &gate, &m, "after the merge.after hooks")?;
    }
    if ok {
        ok = verify_settled(&mut run, &gate, &settings, &m, &settled, &hooks.touched)?;
    }
    if run.interrupted().is_some() {
        return interrupted(run);
    }
    if !ok {
        return red(
            run,
            &integration,
            &gate.root,
            &run_dir,
            started,
            &[],
            Vec::new(),
            m,
        );
    }
    let head = git::run(&gate.root, ["rev-parse", "HEAD^{tree}"])?;
    run.set_tree(head, Some(m.clone()), false);

    // The merged settings decide the platforms; step 1 checked the settings the batch was to
    // produce, so a platform (or a pin) that differs from those is checked for a machine
    // before anything runs.
    let expected = batch_settings(&root, items, old_settings)?;
    for w in waivers {
        if !old_settings.platforms.contains_key(&w.platform)
            && !settings.platforms.contains_key(&w.platform)
        {
            return refuse_in(
                run,
                Failure::refused(format!(
                    "--waive {}: not a platform of {integration}'s settings or the merged commit's (they have: {}); {integration} has not moved",
                    w.platform,
                    platform_names(&settings).join(", ")
                )),
            );
        }
    }
    let names: Vec<String> = platform_names(&settings)
        .into_iter()
        .filter(|p| waived(waivers, p).is_none())
        .collect();
    let changed: Vec<String> = names
        .iter()
        .filter(|p| {
            expected.atelier != settings.atelier
                || expected.platforms.get(*p) != settings.platforms.get(*p)
        })
        .cloned()
        .collect();
    if let Err(e) = require_machines(
        &*platforms,
        &ctx.checkout,
        &settings,
        &changed,
        &integration,
    ) {
        return refuse_in(run, e);
    }

    // Steps 5 and 6.
    let mut records = Vec::new();
    for p in platform_names(&settings) {
        if let Some(w) = waived(waivers, &p) {
            println!("  {p}: WAIVED ({})", w.reason);
        }
    }
    for w in waivers {
        if !settings.platforms.contains_key(&w.platform) {
            println!(
                "  {}: WAIVED ({}); the merged settings no longer have it",
                w.platform, w.reason
            );
        }
    }
    let outcomes = {
        let mut g = GateRun {
            ctx,
            run: &mut run,
            gate: &gate,
            settings: &settings,
            m: &m,
        };
        platforms.run_all(&mut g, &names)
    };
    let outcomes = match outcomes {
        Ok(o) => o,
        Err(e) => return refuse_in(run, e),
    };
    if run.interrupted().is_some() {
        return interrupted(run);
    }
    let mut failed: Vec<(String, String, bisect::Failures)> = Vec::new();
    let mut no_machines = Vec::new();
    for p in platform_names(&settings) {
        if let Some(w) = waived(waivers, &p) {
            records.push(PlatformRecord {
                platform: p.clone(),
                result: "waived".to_owned(),
                machine: None,
                reason: Some(w.reason.clone()),
                run: None,
            });
            continue;
        }
        match outcomes
            .iter()
            .find(|(n, _)| *n == p)
            .map(|(_, o)| o.clone())
        {
            Some(PlatformOutcome::Passed { machine, run }) => {
                records.push(PlatformRecord {
                    platform: p.clone(),
                    result: "pass".to_owned(),
                    machine,
                    reason: None,
                    run,
                });
            }
            Some(PlatformOutcome::Failed { what, failures, .. }) => {
                failed.push((p.clone(), what, failures))
            }
            Some(PlatformOutcome::NoMachine(why) | PlatformOutcome::Cancelled(why)) => {
                no_machines.push(why)
            }
            None => no_machines.push(format!("{p}: not run")),
        }
    }
    if !failed.is_empty() {
        for (p, what, _) in &failed {
            run.fail(format!("{p}: {what}"));
        }
        let lines: Vec<String> = failed
            .iter()
            .map(|(p, w, _)| format!("{p} ({w})"))
            .collect();
        let all: Vec<bisect::Failures> = failed.into_iter().map(|(_, _, f)| f).collect();
        return red(
            run,
            &integration,
            &gate.root,
            &run_dir,
            started,
            &lines,
            all,
            m,
        );
    }
    if !no_machines.is_empty() {
        run.fail(format!("no machine: {}", no_machines.join("; ")));
        let _ = run.finish(false);
        return Err(Failure::new(
            Exit::NoMachine,
            format!(
                "merge: no machine for every platform; {integration} has not moved (run {run_id}):\n  {}",
                no_machines.join("\n  ")
            ),
        ));
    }
    // Step 8 of r4 §3.8: the tree is still M's, and M is built on OLD.
    let mut ok = tree_is(&mut run, &gate, &m, "after the platform runs")?;
    if ok && !is_ancestor(&root, old, &m)? {
        run.fail(format!(
            "{} is not built on {}",
            short(&root, &m),
            short(&root, old)
        ));
        ok = false;
    }
    if !ok {
        return red(
            run,
            &integration,
            &gate.root,
            &run_dir,
            started,
            &[],
            Vec::new(),
            m,
        );
    }

    // Step 7: the record, then the promotion (by the caller, under the same lock).
    let summary = format!(
        "merge: GREEN — run {run_id}, {integration} {} → {}, {}, {}",
        short(&root, old),
        short(&root, &m),
        records
            .iter()
            .map(PlatformRecord::describe)
            .collect::<Vec<_>>()
            .join(", "),
        duration(started.elapsed())
    );
    let rec = Promotion {
        integration: integration.clone(),
        old: old.to_owned(),
        m: m.clone(),
        run: run_id.clone(),
        created: crate::fsutil::rfc3339(crate::fsutil::now_secs()),
        push_started: None,
        push_pgid: None,
        push_ended: None,
        summary: Some(summary),
        rules_changed: items.iter().any(|i| i.rules),
        branches: items
            .iter()
            .map(|i| PromotedBranch {
                name: i.name.clone(),
                lifetime: i.lifetime.clone(),
                commit: i.commit.clone(),
                worktree: i.approval.worktree.clone(),
                dev: i.approval.dev,
                ino: i.approval.ino,
            })
            .collect(),
        platforms: records,
    };
    rec.write(v)?;
    let (_result, lock) = run.finish_keeping_lock(true)?;
    Ok(GateEnd::Green {
        rec: Box::new(rec),
        lock,
        run_dir,
    })
}

/// Step 1's per-branch preconditions, in batch order. Refuses (4), changing nothing.
fn preconditions(
    ctx: &Context,
    v: &crate::state::Versioned,
    stack: &Stack,
    branches: &[String],
    old_settings: &Settings,
) -> Result<Vec<Item>> {
    let root = &ctx.checkout.root;
    let mut items: Vec<Item> = Vec::new();
    for b in branches {
        let approval = Approval::read(v, b)?.ok_or_else(|| {
            Failure::refused(format!(
                "{b} is not approved: atelier approve {b} <the reviewed commit>"
            ))
        })?;
        stack.started(b)?;
        stack.refuse_rebase_open(b)?;
        stack.require_current(b)?;
        let chain = stack.chain(b)?;
        let me = &chain[0];
        if approval.lifetime != me.record.lifetime {
            return Err(Failure::refused(format!(
                "{b} was started again since it was approved: approve it again"
            )));
        }
        if me.tip != approval.commit {
            return Err(Failure::refused(format!(
                "{b} moved since it was approved at {}: it is at {}; review and approve it again",
                short(root, &approval.commit),
                short(root, &me.tip)
            )));
        }
        crate::approve::require_mergeable(stack, root, b)?;
        if let Some(lower) = chain.get(1) {
            let l = &lower.record.name;
            let earlier = items.iter().find(|i| &i.name == l);
            let Some(earlier) = earlier else {
                let later = branches.iter().any(|x| x == l);
                return Err(Failure::refused(if later {
                    format!(
                        "{b} is built on {l}, which must come earlier in the batch: name {l} before {b}"
                    )
                } else {
                    format!(
                        "{b} is built on {l}, which is not merged: merge {l} in the same batch, before {b}"
                    )
                }));
            };
            if !is_ancestor(root, &me.record.base, &earlier.commit)? {
                return Err(Failure::refused(format!(
                    "{b} is built on {} of {l}, which {l}'s approved commit {} does not contain: run atelier follow in {b}'s worktree, or approve {l} again",
                    short(root, &me.record.base),
                    short(root, &earlier.commit)
                )));
            }
        }
        let rules = changes_rules(root, old_settings, &me.record.base, &approval.commit)?;
        items.push(Item {
            name: b.clone(),
            lifetime: me.record.lifetime.clone(),
            commit: approval.commit.clone(),
            approval,
            rules,
        });
    }
    if items.len() > 1
        && let Some(r) = items.iter().find(|i| i.rules)
    {
        return Err(Failure::refused(format!(
            "{} changes .atelier/ or the generated rules: merge {} on its own",
            r.name, r.name
        )));
    }
    Ok(items)
}

/// True when the branch's own commits (from its recorded base) touch `.atelier/`, `AGENTS.md`
/// or a file the rules import into. A stacked branch is judged by its own commits; its lower
/// branch is judged by its own.
fn changes_rules(root: &Path, settings: &Settings, base: &str, commit: &str) -> Result<bool> {
    let out = git::output(
        root,
        [
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            base,
            commit,
            "--",
        ],
    )?;
    if !out.status.success() {
        return Err(Failure::refused(format!(
            "git diff {base} {commit} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(split_z(&out.stdout).iter().any(|p| {
        p.starts_with(".atelier/")
            || p == "AGENTS.md"
            || settings.rules.import_into.iter().any(|f| f == p)
    }))
}

fn platform_names(settings: &Settings) -> Vec<String> {
    settings.platforms.keys().cloned().collect()
}

pub(crate) fn split_z(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|&b| b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .collect()
}

/// Step 2: the gate worktree exists (created detached at `OLD` on first use), is registered at
/// exactly that path, detached, with no git operation in progress and clean (no tracked change,
/// no untracked file git does not ignore); then it is checked out at `OLD` and its tree id must
/// be `OLD`'s tree. Refuses (4), listing what is in the way; it deletes nothing.
pub fn prepare_gate(main: &Path, gate: &Path, old: &str) -> Result<Checkout> {
    match fs::symlink_metadata(gate) {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            let out = git::output(
                main,
                [
                    "worktree",
                    "add",
                    "-q",
                    "--detach",
                    &gate.to_string_lossy(),
                    old,
                ],
            )?;
            if !out.status.success() {
                return Err(Failure::refused(format!(
                    "cannot create the gate worktree {}: {}",
                    gate.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
        }
        Err(e) => return Err(Failure::refused(format!("{}: {e}", gate.display()))),
        Ok(meta) if !meta.is_dir() => {
            return Err(Failure::refused(format!(
                "{} is not a folder: atelier uses that path as the gate worktree; move it away",
                gate.display()
            )));
        }
        Ok(_) => {
            let want = git::canonical(gate);
            let entry = git::worktree_list(main)?
                .into_iter()
                .find(|w| git::canonical(&w.path) == want && !w.main);
            let Some(entry) = entry else {
                return Err(Failure::refused(format!(
                    "{} exists and is not a worktree of this repository: atelier uses that path as the gate worktree; move it away",
                    gate.display()
                )));
            };
            if let Some(b) = &entry.branch {
                return Err(Failure::refused(format!(
                    "the gate worktree {} has {} checked out: atelier keeps it detached (git -C {} switch --detach)",
                    gate.display(),
                    b.strip_prefix("refs/heads/").unwrap_or(b),
                    gate.display()
                )));
            }
        }
    }
    let checkout = Checkout::find(gate)?
        .filter(|c| git::canonical(&c.root) == git::canonical(gate))
        .ok_or_else(|| {
            Failure::refused(format!(
                "{} is not a worktree of this repository",
                gate.display()
            ))
        })?;
    if git::canonical(&checkout.common_dir) != git::canonical(&main_common(main)?) {
        return Err(Failure::refused(format!(
            "{} is a worktree of another repository",
            gate.display()
        )));
    }
    let ops = operations_in_progress(&checkout.git_dir);
    if !ops.is_empty() {
        return Err(Failure::refused(format!(
            "the gate worktree {} has {} in progress: look at it and finish or abort it there; atelier changes nothing in it",
            gate.display(),
            ops.join(" and ")
        )));
    }
    let dirty = unclean_paths(&checkout.root)?;
    if !dirty.is_empty() {
        return Err(Failure::refused(format!(
            "the gate worktree {} is not clean; atelier deletes nothing there. Look at each path, keep what matters and remove the rest:\n  {}",
            gate.display(),
            dirty.join("\n  ")
        )));
    }
    checkout_detached(&checkout.root, old)?;
    let tree_now = tree::tree_id(&checkout)?;
    let old_tree = git::run(&checkout.root, ["rev-parse", &format!("{old}^{{tree}}")])?;
    if tree_now != old_tree {
        return Err(Failure::refused(format!(
            "the gate worktree {} does not hold {}'s tree after checking it out",
            gate.display(),
            short(main, old)
        )));
    }
    Ok(checkout)
}

fn main_common(main: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(git::run(
        main,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?))
}

/// The git operations in progress in a worktree's git folder, and a left `index.lock`.
pub fn operations_in_progress(git_dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (file, what) in [
        ("MERGE_HEAD", "a merge"),
        ("rebase-merge", "a rebase"),
        ("rebase-apply", "a rebase or git am"),
        ("CHERRY_PICK_HEAD", "a cherry-pick"),
        ("REVERT_HEAD", "a revert"),
        ("sequencer", "a cherry-pick or revert sequence"),
        ("BISECT_LOG", "a bisect"),
        ("BISECT_START", "a bisect"),
    ] {
        if fs::symlink_metadata(git_dir.join(file)).is_ok() && !out.iter().any(|o| o == what) {
            out.push(what.to_owned());
        }
    }
    if fs::symlink_metadata(git_dir.join("index.lock")).is_ok() {
        out.push(format!(
            "a git command ({} exists)",
            git_dir.join("index.lock").display()
        ));
    }
    out
}

/// Tracked changes and untracked files git does not ignore, as `git status` names them.
pub fn unclean_paths(dir: &Path) -> Result<Vec<String>> {
    let out = git::output(
        dir,
        ["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    if !out.status.success() {
        return Err(Failure::refused(format!(
            "git status in {} failed: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let mut paths = Vec::new();
    let mut fields = out.stdout.split(|&b| b == 0).filter(|f| !f.is_empty());
    while let Some(f) = fields.next() {
        let text = String::from_utf8_lossy(f).into_owned();
        let (xy, path) = text.split_at(text.len().min(3));
        let xy = xy.trim();
        if xy.starts_with('R') || xy.starts_with('C') {
            fields.next();
        }
        paths.push(match xy {
            "??" => format!("{path} (untracked)"),
            _ => format!("{path} ({xy})"),
        });
    }
    Ok(paths)
}

/// `git checkout --detach <commit>` in the gate, refusing to overwrite an ignored file.
pub fn checkout_detached(dir: &Path, commit: &str) -> Result<()> {
    let out = git::output(
        dir,
        [
            "checkout",
            "-q",
            "--detach",
            "--no-overwrite-ignore",
            commit,
            "--",
        ],
    )?;
    if !out.status.success() {
        return Err(Failure::refused(format!(
            "git checkout {commit} in {} failed: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// How one merge of step 3 ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merged {
    Clean,
    /// Conflicts only in regenerated paths, settled with the incoming side.
    Settled(Vec<String>),
    /// A conflict in these paths; the merge was aborted.
    Conflict(Vec<String>),
}

/// The configuration every git command of the gate's merges carries: no global attributes
/// file (with `GIT_ATTR_NOSYSTEM=1`, no system one either), no reused resolutions, no
/// renormalization, renames found.
pub(crate) const GATE_CONFIG: &[&str] = &[
    "-c",
    "core.attributesFile=/dev/null",
    "-c",
    "rerere.enabled=false",
    "-c",
    "merge.renormalize=false",
    "-c",
    "merge.renames=true",
];

/// git for the gate's merges: [`GATE_CONFIG`] and `GIT_ATTR_NOSYSTEM=1`.
pub fn gate_git<I, S>(dir: &Path, args: I) -> Result<std::process::Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    git::command(dir)
        .args(GATE_CONFIG)
        .args(args)
        .env("GIT_ATTR_NOSYSTEM", "1")
        .output()
        .map_err(|e| Failure::refused(format!("cannot run git: {e}")))
}

/// The options every gate merge carries: no fast-forward, no hooks, no signing, no autostash,
/// and never an ignored file overwritten.
fn merge_args<'a>(commit: &'a str, message: &'a str) -> Vec<&'a str> {
    vec![
        "merge",
        "-q",
        "--no-ff",
        "--no-edit",
        "--no-verify",
        "--no-gpg-sign",
        "--no-autostash",
        "--no-verify-signatures",
        "--no-overwrite-ignore",
        "-m",
        message,
        commit,
    ]
}

/// The merge drivers the gate lets merge a path both sides changed: git's own text and binary
/// merges, which conflict where both sides changed the same lines. `union` is git's own too,
/// but it never conflicts: it keeps both sides' lines, settling every overlap without a word.
/// Any other driver runs a command from the git configuration.
const BUILT_IN_DRIVERS: &[&str] = &["text", "binary"];

/// A value of the effective git configuration in `dir`, if set.
fn config_value(dir: &Path, key: &str) -> Result<Option<String>> {
    let out = git::output(dir, ["config", "--get", key])?;
    Ok(out
        .status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned()))
}

/// The paths changed on both sides of merging `commit` into `head`.
fn changed_on_both_sides(dir: &Path, head: &str, commit: &str) -> Result<Vec<String>> {
    let base = git::output(dir, ["merge-base", head, commit])?;
    if !base.status.success() {
        return Ok(Vec::new());
    }
    let base = String::from_utf8_lossy(&base.stdout).trim().to_owned();
    let changed = |to: &str| -> Result<BTreeSet<String>> {
        let out = git::output(
            dir,
            ["diff", "--name-only", "--no-renames", "-z", &base, to, "--"],
        )?;
        Ok(split_z(&out.stdout).into_iter().collect())
    };
    let ours = changed(head)?;
    Ok(changed(commit)?.intersection(&ours).cloned().collect())
}

/// The `merge` attribute of each path, as `git check-attr` gives it with the gate's
/// configuration: from the checkout's `.gitattributes`, or `commit`'s with `source`, and
/// `info/attributes`.
fn merge_attributes(
    dir: &Path,
    source: Option<&str>,
    paths: &[String],
) -> Result<Vec<(String, String)>> {
    let mut cmd = git::command(dir);
    cmd.args(GATE_CONFIG)
        .args(["check-attr", "-z", "--stdin"])
        .env("GIT_ATTR_NOSYSTEM", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(s) = source {
        cmd.arg(format!("--source={s}"));
    }
    cmd.arg("merge");
    let mut child = cmd
        .spawn()
        .map_err(|e| Failure::refused(format!("cannot run git: {e}")))?;
    {
        use std::io::Write as _;
        let mut stdin = child.stdin.take().expect("piped");
        for p in paths {
            let _ = stdin.write_all(p.as_bytes());
            let _ = stdin.write_all(b"\0");
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| Failure::refused(format!("git check-attr: {e}")))?;
    if !out.status.success() {
        return Err(Failure::refused(format!(
            "git check-attr merge failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let fields = split_z(&out.stdout);
    Ok(fields
        .chunks(3)
        .filter(|c| c.len() == 3)
        .map(|c| (c[0].clone(), c[2].clone()))
        .collect())
}

/// Refuses (4) a merge in which a path changed on both sides would be merged by a merge
/// driver other than git's text and binary merges (`BUILT_IN_DRIVERS`): `union` or a
/// configured driver, which `.gitattributes` (either side's) or `info/attributes` assigns or
/// `merge.default` names, or a built-in name the git configuration redefines. Such a driver
/// could settle a conflict without a word (DESIGN §8.2: the gate never resolves anything).
pub fn refuse_merge_drivers(dir: &Path, head: &str, commit: &str, name: &str) -> Result<()> {
    let both = changed_on_both_sides(dir, head, commit)?;
    if both.is_empty() {
        return Ok(());
    }
    let default = config_value(dir, "merge.default")?;
    let mut problems: Vec<String> = Vec::new();
    for source in [None, Some(commit)] {
        for (path, value) in merge_attributes(dir, source, &both)? {
            let driver = match value.as_str() {
                // `merge` set or unset: git's text and binary merges, never a configured one.
                "set" | "unset" => continue,
                "unspecified" => match &default {
                    Some(d) => d.clone(),
                    None => continue,
                },
                other => other.to_owned(),
            };
            let line = if driver == "union" {
                format!("{path}: merge=union, which keeps both sides' lines and never conflicts")
            } else if !BUILT_IN_DRIVERS.contains(&driver.as_str()) {
                format!("{path}: merge driver {driver:?}")
            } else if config_value(dir, &format!("merge.{driver}.driver"))?.is_some() {
                format!(
                    "{path}: merge={driver}, which the git configuration redefines (merge.{driver}.driver)"
                )
            } else {
                continue;
            };
            if !problems.contains(&line) {
                problems.push(line);
            }
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(Failure::refused(format!(
        "merging {name}: paths changed on both sides would be merged by a merge driver other than git's text and binary merges; the gate never lets anything resolve a conflict:\n  {}\nremove that merge attribute (.gitattributes, .git/info/attributes) or merge.default, or the driver from the git configuration; nothing was merged",
        problems.join("\n  ")
    )))
}

/// Merges one approved commit into the gate's `HEAD` (step 3). `old_regen` is
/// [`regenerated_paths`] of OLD's committed settings; a rules item settles nothing.
pub fn merge_one(gate: &Path, item: &Item, old_regen: &BTreeSet<String>) -> Result<Merged> {
    let none = BTreeSet::new();
    let regen = if item.rules { &none } else { old_regen };
    merge_commit(gate, &item.commit, &item.name, regen)
}

/// Merges `commit` (the branch `name`'s) into the gate's `HEAD`. A conflict is settled only in
/// paths both `old_regen` and the merged settings (the index's, after the merge) name in a
/// hook with `commit`; the incoming side is taken there.
pub fn merge_commit(
    gate: &Path,
    commit: &str,
    name: &str,
    old_regen: &BTreeSet<String>,
) -> Result<Merged> {
    refuse_merge_drivers(gate, "HEAD", commit, name)?;
    let message = format!("Merge branch '{name}'");
    let out = gate_git(gate, merge_args(commit, &message))?;
    if out.status.success() {
        return Ok(Merged::Clean);
    }
    let conflicted =
        split_z(&git::output(gate, ["diff", "--name-only", "--diff-filter=U", "-z"])?.stdout);
    let in_merge = git::output(gate, ["rev-parse", "-q", "--verify", "MERGE_HEAD"])?
        .status
        .success();
    if conflicted.is_empty() {
        if in_merge {
            let _ = gate_git(gate, ["merge", "--abort"]);
        }
        return Err(Failure::refused(format!(
            "git merge of {name} in the gate failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let regen: BTreeSet<String> = if old_regen.is_empty() {
        BTreeSet::new()
    } else {
        match settings_at(gate, "") {
            Ok(Some(merged)) => regenerated_paths(&merged)
                .intersection(old_regen)
                .cloned()
                .collect(),
            _ => BTreeSet::new(),
        }
    };
    let real: Vec<String> = conflicted
        .iter()
        .filter(|p| !regen.contains(*p))
        .cloned()
        .collect();
    if !real.is_empty() {
        abort(gate)?;
        return Ok(Merged::Conflict(real));
    }
    for p in &conflicted {
        let theirs = gate_git(gate, ["checkout", "--theirs", "--", p])?;
        let staged = if theirs.status.success() {
            gate_git(gate, ["add", "--", p])?
        } else {
            // The incoming side deleted it.
            gate_git(gate, ["rm", "-q", "-f", "--", p])?
        };
        if !staged.status.success() {
            abort(gate)?;
            return Err(Failure::refused(format!(
                "cannot take {name}'s side of {p}: {}",
                String::from_utf8_lossy(&staged.stderr).trim()
            )));
        }
    }
    let commit_out = gate_git(
        gate,
        ["commit", "-q", "--no-edit", "--no-verify", "--no-gpg-sign"],
    )?;
    if !commit_out.status.success() {
        abort(gate)?;
        return Err(Failure::refused(format!(
            "cannot finish the merge of {name}: {}",
            String::from_utf8_lossy(&commit_out.stderr).trim()
        )));
    }
    Ok(Merged::Settled(conflicted))
}

fn abort(gate: &Path) -> Result<()> {
    let out = gate_git(gate, ["merge", "--abort"])?;
    if !out.status.success() {
        return Err(Failure::refused(format!(
            "git merge --abort in the gate failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

/// What a conflicting branch conflicts with: `main` when its approved commit alone conflicts
/// with `OLD`, otherwise the earlier branches of the batch it conflicts with.
fn conflict_with(
    dir: &Path,
    old: &str,
    earlier: &[Item],
    item: &Item,
    integration: &str,
    regen: &BTreeSet<String>,
) -> String {
    if earlier.is_empty() || trees_conflict(dir, old, &item.commit, regen) {
        return integration.to_owned();
    }
    let with: Vec<&str> = earlier
        .iter()
        .filter(|e| trees_conflict(dir, &e.commit, &item.commit, regen))
        .map(|e| e.name.as_str())
        .collect();
    if with.is_empty() {
        format!(
            "{} (earlier in this batch, together)",
            earlier
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        format!("{} (earlier in this batch)", with.join(", "))
    }
}

/// True when merging `a` and `b` conflicts in a path no hook regenerates (`git merge-tree`,
/// touching no files).
fn trees_conflict(dir: &Path, a: &str, b: &str, regen: &BTreeSet<String>) -> bool {
    let Ok(out) = gate_git(
        dir,
        [
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            "-z",
            a,
            b,
        ],
    ) else {
        return true;
    };
    match out.status.code() {
        Some(0) => false,
        Some(1) => split_z(&out.stdout)
            .into_iter()
            .skip(1)
            .any(|p| !regen.contains(&p)),
        _ => true,
    }
}

/// How step 4's hooks ended.
pub struct HooksEnd {
    /// Every hook passed and left nothing uncommitted.
    pub ok: bool,
    /// Per hook with `commit`, the paths its commit changed.
    pub touched: std::collections::BTreeMap<String, BTreeSet<String>>,
}

/// Step 4: the `merge.after` hooks in the gate.
pub(crate) fn after_hooks(run: &mut Run, gate: &Path, settings: &Settings) -> Result<HooksEnd> {
    let mut end = HooksEnd {
        ok: false,
        touched: Default::default(),
    };
    for h in &settings.merge.after {
        let rec = run.step(&format!("after:{}", h.name), &h.run, gate, &[])?;
        if !rec.passed() {
            println!("  merge.after {}: FAILED (log {})", h.name, rec.log);
            run.fail(format!("merge.after {} failed (log {})", h.name, rec.log));
            return Ok(end);
        }
        if run.interrupted().is_some() {
            return Ok(end);
        }
        let changed = unclean_paths(gate)?;
        match (&h.commit, changed.is_empty()) {
            (_, true) => println!("  merge.after {}: ok, nothing changed", h.name),
            (Some(msg), false) => {
                let add = git::output(gate, ["add", "-A", "--", "."])?;
                let commit = if add.status.success() {
                    git::output(
                        gate,
                        ["commit", "-q", "--no-verify", "--no-gpg-sign", "-m", msg],
                    )?
                } else {
                    add
                };
                if !commit.status.success() {
                    run.fail(format!(
                        "merge.after {}: its changes could not be committed: {}",
                        h.name,
                        String::from_utf8_lossy(&commit.stderr).trim()
                    ));
                    return Ok(end);
                }
                let paths = git::output(
                    gate,
                    [
                        "diff-tree",
                        "--no-commit-id",
                        "--name-only",
                        "--no-renames",
                        "-r",
                        "-z",
                        "HEAD",
                    ],
                )?;
                end.touched
                    .entry(h.name.clone())
                    .or_default()
                    .extend(split_z(&paths.stdout));
                println!(
                    "  merge.after {}: committed {} path(s): {msg}",
                    h.name,
                    changed.len()
                );
            }
            (None, false) => {
                println!("  merge.after {}: changed the tree", h.name);
                run.fail(format!(
                    "merge.after {} changed the tree and has no commit message: {}",
                    h.name,
                    changed.join(", ")
                ));
                return Ok(end);
            }
        }
    }
    end.ok = true;
    Ok(end)
}

/// After step 4, each conflict settled with the incoming side (HF1) must hold what a hook of
/// the merged settings with `commit` that names it produced: its commit changed the path, or,
/// with the path removed, running the hooks that name it again gives back exactly `M`'s tree.
/// Otherwise RED: the path would hold one side's hand edit, not the generated content.
pub(crate) fn verify_settled(
    run: &mut Run,
    gate: &Checkout,
    settings: &Settings,
    m: &str,
    settled: &BTreeSet<String>,
    touched: &std::collections::BTreeMap<String, BTreeSet<String>>,
) -> Result<bool> {
    let mut again: Vec<String> = Vec::new();
    let mut paths: Vec<&String> = Vec::new();
    for p in settled {
        let hooks: Vec<&crate::settings::AfterHook> = settings
            .merge
            .after
            .iter()
            .filter(|h| h.commit.is_some() && h.regenerates.contains(p))
            .collect();
        if hooks.is_empty() {
            run.fail(format!(
                "the conflict in {p} was settled for a merge.after hook, but no hook with commit names it in the merged settings"
            ));
            return Ok(false);
        }
        if hooks
            .iter()
            .any(|h| touched.get(&h.name).is_some_and(|t| t.contains(p)))
        {
            continue;
        }
        paths.push(p);
        for h in hooks {
            if !again.contains(&h.name) {
                again.push(h.name.clone());
            }
        }
    }
    if paths.is_empty() {
        return Ok(true);
    }
    println!(
        "  regenerated paths not rewritten by their hook's commit, checked by running it again: {}",
        paths
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    for p in &paths {
        match fs::remove_file(gate.root.join(p)) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => {
                run.fail(format!("{p}: {e}"));
                return Ok(false);
            }
        }
    }
    let mut ok = true;
    for h in settings
        .merge
        .after
        .iter()
        .filter(|h| again.contains(&h.name))
    {
        let rec = run.step(&format!("regenerate:{}", h.name), &h.run, &gate.root, &[])?;
        if !rec.passed() {
            run.fail(format!(
                "merge.after {} failed when run again (log {})",
                h.name, rec.log
            ));
            ok = false;
            break;
        }
    }
    if ok {
        let now = tree::tree_id(gate).ok();
        let want = git::run(&gate.root, ["rev-parse", &format!("{m}^{{tree}}")])?;
        if now.as_deref() != Some(want.as_str()) {
            let files = unclean_paths(&gate.root).unwrap_or_default();
            run.fail(format!(
                "merge.after {} did not regenerate {} as merged: running it again gives another tree ({}); a regenerates hook must always rewrite its files",
                again.join(", "),
                paths
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                files.join(", ")
            ));
            ok = false;
        }
    }
    if !ok {
        // Back what atelier removed and no hook wrote again; anything else stays for inspection.
        let missing: Vec<&str> = paths
            .iter()
            .filter(|p| fs::symlink_metadata(gate.root.join(p)).is_err())
            .map(|p| p.as_str())
            .collect();
        if !missing.is_empty() {
            let mut args = vec!["checkout", "-q", m, "--"];
            args.extend(missing);
            let _ = git::output(&gate.root, args);
        }
    }
    Ok(ok)
}

/// The gate's tree id must be `m`'s tree; a failure line otherwise.
fn tree_is(run: &mut Run, gate: &Checkout, m: &str, when: &str) -> Result<bool> {
    let now = match tree::tree_id(gate) {
        Ok(t) => t,
        Err(e) => {
            run.fail(format!("tree {when}: {}", e.message));
            return Ok(false);
        }
    };
    let want = git::run(&gate.root, ["rev-parse", &format!("{m}^{{tree}}")])?;
    if now != want {
        let files = unclean_paths(&gate.root).unwrap_or_default();
        run.fail(format!(
            "the gate's tree is not the merged commit's {when}: {}",
            files.join(", ")
        ));
        return Ok(false);
    }
    Ok(true)
}

/// A platform run on this computer, in the gate worktree on `M` (DESIGN §2.5).
pub fn run_local(g: &mut GateRun<'_>, home: &Path, platform: &str) -> Result<PlatformOutcome> {
    let runs_dir = g.ctx.state.runs_dir();
    run_local_in(
        g.run,
        &runs_dir,
        &g.gate.root,
        g.settings,
        home,
        platform,
        None,
    )
}

/// Runs `platform`'s `run` in the gate folder as steps of `run`: `atelier check` through the
/// pinned atelier of `settings` (with `only`, the narrow re-run of bisection), or the
/// platform's command after the `check.prepare` hooks that apply to it and its own `prepare`.
pub fn run_local_in(
    run: &mut Run,
    runs_dir: &Path,
    gate: &Path,
    settings: &Settings,
    home: &Path,
    platform: &str,
    only: Option<&crate::check::Only>,
) -> Result<PlatformOutcome> {
    let failed = |what: String, run_id: Option<String>, failures: bisect::Failures| {
        println!("  {platform}: FAILED ({what})");
        PlatformOutcome::Failed {
            machine: None,
            run: run_id,
            what,
            failures,
        }
    };
    let whole = || bisect::Failures::whole(platform);
    let Some(ps) = settings.platforms.get(platform).cloned() else {
        return Ok(failed(
            format!("{platform} is not a platform of the merged settings"),
            None,
            whole(),
        ));
    };
    let job_env: Vec<(String, String)> = crate::nextest::JOB_ENV
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    match &ps.run {
        PlatformRun::Named(_) => {
            let pin = settings.atelier.clone();
            let bin = dispatch::binary_for(home, &pin);
            if !bin.is_file() {
                return Ok(failed(
                    format!(
                        "atelier {pin}, pinned by the merged commit, is not installed: {}",
                        dispatch::install_command(&pin)
                    ),
                    None,
                    whole(),
                ));
            }
            let mut argv = vec![bin.to_string_lossy().into_owned(), "check".to_owned()];
            let mut name = format!("check@{platform}");
            if let Some(o) = only {
                let file = run
                    .dir()
                    .join(format!("only-{platform}-{}.toml", run.steps().len()));
                let text = toml::to_string(o).map_err(|e| Failure::refused(e.to_string()))?;
                crate::fsutil::write_atomic(&file, text.as_bytes())
                    .map_err(|e| Failure::refused(format!("{}: {e}", file.display())))?;
                argv.push("--only".to_owned());
                argv.push(file.to_string_lossy().into_owned());
                name = format!("check-only@{platform}");
            }
            // The check takes the id it is given, so its run is known exactly; a check step that
            // runs atelier itself starts runs of its own under it.
            let wanted = crate::runs::new_run_id()?;
            let env = vec![
                (DISPATCHED_VAR.to_owned(), "1".to_owned()),
                (crate::runs::RUN_ID_VAR.to_owned(), wanted.clone()),
            ];
            let before = direct_runs_under(runs_dir, run.id());
            let rec = run.step(&name, &argv, gate, &env)?;
            let inner = if direct_runs_under(runs_dir, run.id()).contains(&wanted) {
                Some(wanted)
            } else {
                // A pinned atelier that does not take the id: the one new run directly under
                // this one, if there is exactly one.
                let new: Vec<String> = direct_runs_under(runs_dir, run.id())
                    .into_iter()
                    .filter(|id| !before.contains(id))
                    .collect();
                (new.len() == 1).then(|| new[0].clone())
            };
            let result = inner
                .as_ref()
                .and_then(|id| RunResult::read(&runs_dir.join(id)).ok().flatten());
            let passed = rec.passed()
                && result
                    .as_ref()
                    .is_some_and(|r| r.outcome == Outcome::Passed);
            if passed {
                println!(
                    "  {platform}: PASS on this computer (check run {})",
                    inner.as_deref().unwrap_or("?")
                );
                return Ok(PlatformOutcome::Passed {
                    machine: None,
                    run: inner,
                });
            }
            Ok(match (&result, &inner) {
                (Some(r), Some(id)) => failed(
                    format!("check run {id}: {}", failed_names(r).join(", ")),
                    inner.clone(),
                    bisect::Failures::from_check(platform, &runs_dir.join(id), r),
                ),
                _ => failed(
                    format!(
                        "atelier check did not run to a result (exit {}, log {})",
                        rec.exit
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "-".into()),
                        rec.log
                    ),
                    inner.clone(),
                    whole(),
                ),
            })
        }
        PlatformRun::Command(argv) => {
            let this = Platform::parse(platform);
            let mut steps: Vec<(String, Vec<String>)> = Vec::new();
            for h in &settings.check.prepare {
                let applies = h
                    .platforms
                    .as_ref()
                    .is_none_or(|l| l.iter().any(|p| Platform::parse(p) == this));
                if applies {
                    steps.push((format!("prepare:{}@{platform}", h.name), h.run.clone()));
                }
            }
            if let Some(p) = &ps.prepare {
                steps.push((format!("prepare@{platform}"), p.clone()));
            }
            steps.push((format!("run@{platform}"), argv.clone()));
            let id = run.id().to_owned();
            for (name, argv) in steps {
                let rec = run.step(&name, &argv, gate, &job_env)?;
                if !rec.passed() {
                    return Ok(failed(
                        format!("{name} failed (log {})", rec.log),
                        Some(id),
                        whole(),
                    ));
                }
                if run.interrupted().is_some() {
                    return Ok(failed("interrupted".to_owned(), Some(id), whole()));
                }
            }
            println!("  {platform}: PASS on this computer");
            Ok(PlatformOutcome::Passed {
                machine: None,
                run: Some(id),
            })
        }
    }
}

/// The names of what failed in a check's result: its failed steps, then its other failures.
fn failed_names(r: &RunResult) -> Vec<String> {
    let mut out: Vec<String> = r
        .steps
        .iter()
        .filter(|s| s.status != StepStatus::Passed)
        .map(|s| s.name.clone())
        .collect();
    if out.is_empty() {
        out.extend(r.failures.iter().cloned());
    }
    if out.is_empty() {
        out.push("see its log".to_owned());
    }
    out
}

/// The runs started directly by a step of the run `outer`: their `ATELIER_RUNS` list ends with
/// `outer`, then their own id.
pub fn direct_runs_under(runs_dir: &Path, outer: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let Ok(entries) = fs::read_dir(runs_dir) else {
        return found;
    };
    for e in entries.flatten() {
        if let Ok(Some(info)) = RunInfo::read(&e.path()) {
            let n = info.runs.len();
            if n >= 2 && info.runs[n - 2] == outer && info.runs[n - 1] == info.id {
                found.insert(info.id);
            }
        }
    }
    found
}

/// A RED gate (step 6): the result written, the line printed, the gate kept for inspection;
/// the gate lock stays held for bisection.
#[allow(clippy::too_many_arguments)]
fn red(
    run: Run,
    integration: &str,
    gate: &Path,
    run_dir: &Path,
    started: Instant,
    failed: &[String],
    failures: Vec<bisect::Failures>,
    m: String,
) -> Result<GateEnd> {
    let id = run.id().to_owned();
    let (result, lock) = run.finish_keeping_lock(false)?;
    let cause = result.failures.join("; ");
    let what = if failed.is_empty() {
        cause.clone()
    } else {
        failed.join(", ")
    };
    println!(
        "merge: RED — run {id}, failed: {what}; {integration} has not moved; the gate worktree {} is kept for inspection, {}; log {}",
        gate.display(),
        duration(started.elapsed()),
        run_dir.display()
    );
    Ok(GateEnd::Red {
        lock,
        failures,
        run_id: id,
        m,
        cause,
    })
}

/// A stop signal reached the gate before promotion: the run ends, nothing moved.
fn interrupted(run: Run) -> Result<GateEnd> {
    let sig = run.interrupted();
    let id = run.id().to_owned();
    let _ = run.finish(false);
    println!(
        "merge: interrupted by {} — run {id}; nothing moved",
        sig.map(crate::signals::name).unwrap_or("a signal")
    );
    Ok(GateEnd::Done(Exit::Fail))
}
