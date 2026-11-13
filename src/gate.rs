//! `atelier merge <branch>… [--waive <platform> --reason "<owner's words>"]` (DESIGN §3 `merge`,
//! r4 §3.8): merges approved branches in the gate worktree, checks the merged tree on every
//! platform, and moves the integration branch only when that tree is green and nobody moved the
//! branch meanwhile (mode A, `promote`).
//!
//! 1. Under the gate lock: a pending promotion is settled first (the three questions) or
//!    refused; mode A's checks a and b; each branch approved, still at its approved commit,
//!    current, with no merge commit of its own; a lower branch lands only when it is in the
//!    batch, earlier, approved at a commit that contains the upper branch's base (DESIGN §2.4);
//!    a branch that changes `.atelier/` or the generated rules merges alone; every platform has
//!    a machine or is waived.
//! 2. The gate worktree `<main checkout>.gate` must be clean; atelier deletes nothing there.
//!    It is checked out at `OLD`, the integration branch's commit.
//! 3. Each approved commit is merged with `--no-ff`, in order. A conflict only in paths a
//!    `merge.after` hook regenerates is settled by taking the incoming branch's side for those
//!    paths (HF1); any other conflict aborts, returns the gate to `OLD` and exits 5 naming what
//!    the branch conflicts with.
//! 4. The `merge.after` hooks; a hook with `commit` has its changes committed, any other change
//!    is RED. The result is `M`.
//! 5. Every platform's `run` for `M`: here, a platform this computer runs ([`LocalPlatforms`]);
//!    a platform of another machine goes through [`remote_platform`], the seam module 3.6
//!    fills. A `run = "check"` platform runs `atelier check` through `M`'s own pinned atelier.
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
}

/// Where a platform runs: this computer, or another machine (module 3.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Local,
    /// No machine is available; the reason.
    None(String),
}

/// Runs platforms for the gate. [`LocalPlatforms`] is the only one until module 3.6, whose
/// machines file and remote runs replace it: its `availability` chooses among the owner's
/// machines (offline, not ready, busy) and its `run` starts every remote platform at the same
/// time as the local one.
pub trait Platforms {
    /// Checked before the gate starts, in seconds (step 1).
    fn availability(&self, platform: &str) -> Availability;
    /// Runs `platform`'s `run` for the merged commit checked out in the gate (step 5).
    fn run(&mut self, gate: &mut GateRun<'_>, platform: &str) -> Result<PlatformOutcome>;
}

/// The platforms this computer runs, in the gate worktree; every other platform goes to
/// [`remote_platform`].
pub struct LocalPlatforms {
    pub home: PathBuf,
}

impl Platforms for LocalPlatforms {
    fn availability(&self, platform: &str) -> Availability {
        if is_local(platform) {
            Availability::Local
        } else {
            remote_availability(platform)
        }
    }

    fn run(&mut self, gate: &mut GateRun<'_>, platform: &str) -> Result<PlatformOutcome> {
        if is_local(platform) {
            run_local(gate, &self.home, platform)
        } else {
            remote_platform(gate, platform)
        }
    }
}

pub(crate) fn is_local(platform: &str) -> bool {
    Platform::parse(platform).is_some() && Platform::current() == Platform::parse(platform)
}

/// SEAM for module 3.6: whether a machine of another platform can take the run. Until the
/// machines module is merged, none can.
pub fn remote_availability(platform: &str) -> Availability {
    Availability::None(format!(
        "{platform}: no machine (remote machines come with module 3.6)"
    ))
}

/// SEAM for module 3.6: runs `platform`'s `run` for `M` on the machine chosen for it, through
/// `M`'s own pinned atelier there. Until the machines module is merged, no machine is
/// available.
pub fn remote_platform(_gate: &mut GateRun<'_>, platform: &str) -> Result<PlatformOutcome> {
    match remote_availability(platform) {
        Availability::None(why) => Ok(PlatformOutcome::NoMachine(why)),
        Availability::Local => unreachable!("remote_availability never answers Local"),
    }
}

/// The gate's run and what platform runs need from it.
pub struct GateRun<'a> {
    pub ctx: &'a Context,
    pub run: Run,
    /// The gate worktree.
    pub gate: Checkout,
    /// The merged commit's settings.
    pub settings: Settings,
    /// The merged commit.
    pub m: String,
}

/// `atelier merge`.
pub fn run(ctx: &Context, branches: &[String], waiver: Option<Waiver>) -> Result<Exit> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Failure::refused("HOME is not set"))?;
    let mut platforms = LocalPlatforms { home };
    run_with(ctx, branches, waiver, &mut platforms)
}

/// `atelier merge` with the given platform runner.
pub fn run_with(
    ctx: &Context,
    branches: &[String],
    waiver: Option<Waiver>,
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
    if let Some(w) = &waiver {
        if !ctx.settings.platforms.contains_key(&w.platform) {
            return Err(Failure::refused(format!(
                "--waive {}: not a platform of this project (it has: {})",
                w.platform,
                platform_names(&ctx.settings).join(", ")
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
    let items = preconditions(ctx, &v, &stack, branches, &old)?;

    // Machines first, so an unavailable platform costs seconds, not a whole run.
    let mut missing = Vec::new();
    for p in platform_names(&ctx.settings) {
        if waiver.as_ref().is_some_and(|w| w.platform == p) {
            continue;
        }
        if let Availability::None(why) = platforms.availability(&p) {
            missing.push(why);
        }
    }
    if !missing.is_empty() {
        return Err(Failure::new(
            Exit::NoMachine,
            format!(
                "no machine for every platform; nothing ran and {integration} has not moved:\n  {}\nwaive a platform with --waive <platform> --reason \"<the owner's words>\"",
                missing.join("\n  ")
            ),
        ));
    }

    let end = gate(
        ctx,
        &v,
        lock,
        &items,
        &old,
        waiver.as_ref(),
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
        } => {
            if items.len() < 2 {
                return Ok(Exit::Fail);
            }
            let search = bisect::Search {
                ctx,
                items: &items,
                old: &old,
                red_m: &m,
                red_run: &run_id,
                failures: &failures,
            };
            let (lock, green) = search.run(lock)?;
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
                waiver.as_ref(),
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
        failures: Vec<bisect::Failures>,
        run_id: String,
        m: String,
    },
    /// Ended otherwise (interrupted); the lock is released.
    Done(Exit),
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
    waiver: Option<&Waiver>,
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
    let regen = regenerated_paths(&ctx.settings);
    for (i, item) in items.iter().enumerate() {
        match merge_one(&gate.root, item, &regen) {
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
                }
            }
            Ok(Merged::Conflict(files)) => {
                let back = checkout_detached(&gate.root, old);
                let with = conflict_with(&gate.root, old, &items[..i], item, &integration, &regen);
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
                run.fail(e.message.clone());
                let _ = run.finish(false);
                return Err(e);
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
    let mut ok = after_hooks(&mut run, &gate.root, &settings)?;
    if run.interrupted().is_some() {
        return interrupted(run);
    }
    let m = git::run(&gate.root, ["rev-parse", "HEAD"])?;
    if ok {
        ok = tree_is(&mut run, &gate, &m, "after the merge.after hooks")?;
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

    // Steps 5 and 6.
    let mut g = GateRun {
        ctx,
        run,
        gate: gate.clone(),
        settings,
        m: m.clone(),
    };
    let mut records = Vec::new();
    let mut failed: Vec<(String, String, bisect::Failures)> = Vec::new();
    let mut no_machine = Vec::new();
    for p in platform_names(&g.settings) {
        if let Some(w) = waiver.filter(|w| w.platform == p) {
            println!("  {p}: WAIVED ({})", w.reason);
            records.push(PlatformRecord {
                platform: p.clone(),
                result: "waived".to_owned(),
                machine: None,
                reason: Some(w.reason.clone()),
                run: None,
            });
            continue;
        }
        match platforms.run(&mut g, &p)? {
            PlatformOutcome::Passed { machine, run } => {
                records.push(PlatformRecord {
                    platform: p.clone(),
                    result: "pass".to_owned(),
                    machine,
                    reason: None,
                    run,
                });
            }
            PlatformOutcome::Failed { what, failures, .. } => {
                failed.push((p.clone(), what, failures))
            }
            PlatformOutcome::NoMachine(why) => no_machine.push(why),
        }
        if g.run.interrupted().is_some() {
            return interrupted(g.run);
        }
    }
    let GateRun { mut run, .. } = g;
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
    if !no_machine.is_empty() {
        run.fail(format!("no machine: {}", no_machine.join("; ")));
        let _ = run.finish(false);
        return Err(Failure::new(
            Exit::NoMachine,
            format!(
                "merge: no machine for every platform; {integration} has not moved (run {run_id}):\n  {}",
                no_machine.join("\n  ")
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
    old: &str,
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
        let rules = changes_rules(root, &ctx.settings, old, &approval.commit)?;
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

/// True when the branch's own changes (from its merge base with `OLD`) touch `.atelier/`,
/// `AGENTS.md` or a file the rules import into.
fn changes_rules(root: &Path, settings: &Settings, old: &str, commit: &str) -> Result<bool> {
    let out = git::output(
        root,
        [
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            &format!("{old}...{commit}"),
            "--",
        ],
    )?;
    if !out.status.success() {
        return Err(Failure::refused(format!(
            "git diff {old}...{commit} failed: {}",
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

/// Every path some `merge.after` hook regenerates.
pub fn regenerated_paths(settings: &Settings) -> BTreeSet<String> {
    settings
        .merge
        .after
        .iter()
        .flat_map(|h| h.regenerates.iter().cloned())
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

/// The options every gate merge carries: no fast-forward, no hooks, no signing, no autostash,
/// no reused resolutions, and never an ignored file overwritten.
fn merge_args<'a>(commit: &'a str, message: &'a str) -> Vec<&'a str> {
    vec![
        "-c",
        "rerere.enabled=false",
        "merge",
        "-q",
        "--no-ff",
        "--no-edit",
        "--no-verify",
        "--no-gpg-sign",
        "--no-autostash",
        "--no-overwrite-ignore",
        "-m",
        message,
        commit,
    ]
}

/// Merges one approved commit into the gate's `HEAD` (step 3).
pub fn merge_one(gate: &Path, item: &Item, regen: &BTreeSet<String>) -> Result<Merged> {
    merge_commit(gate, &item.commit, &item.name, regen)
}

/// Merges `commit` (the branch `name`'s) into the gate's `HEAD`.
pub fn merge_commit(
    gate: &Path,
    commit: &str,
    name: &str,
    regen: &BTreeSet<String>,
) -> Result<Merged> {
    let message = format!("Merge branch '{name}'");
    let out = git::output(gate, merge_args(commit, &message))?;
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
            let _ = git::output(gate, ["merge", "--abort"]);
        }
        return Err(Failure::refused(format!(
            "git merge of {name} in the gate failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
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
        let theirs = git::output(gate, ["checkout", "--theirs", "--", p])?;
        let staged = if theirs.status.success() {
            git::output(gate, ["add", "--", p])?
        } else {
            // The incoming side deleted it.
            git::output(gate, ["rm", "-q", "-f", "--", p])?
        };
        if !staged.status.success() {
            abort(gate)?;
            return Err(Failure::refused(format!(
                "cannot take {name}'s side of {p}: {}",
                String::from_utf8_lossy(&staged.stderr).trim()
            )));
        }
    }
    let commit_out = git::output(
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
    let out = git::output(gate, ["merge", "--abort"])?;
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
    let Ok(out) = git::output(
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

/// Step 4: the `merge.after` hooks in the gate. Returns whether all passed.
pub(crate) fn after_hooks(run: &mut Run, gate: &Path, settings: &Settings) -> Result<bool> {
    for h in &settings.merge.after {
        let rec = run.step(&format!("after:{}", h.name), &h.run, gate, &[])?;
        if !rec.passed() {
            println!("  merge.after {}: FAILED (log {})", h.name, rec.log);
            run.fail(format!("merge.after {} failed (log {})", h.name, rec.log));
            return Ok(false);
        }
        if run.interrupted().is_some() {
            return Ok(false);
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
                    return Ok(false);
                }
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
                return Ok(false);
            }
        }
    }
    Ok(true)
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
fn run_local(g: &mut GateRun<'_>, home: &Path, platform: &str) -> Result<PlatformOutcome> {
    let gate = g.gate.root.clone();
    let runs_dir = g.ctx.state.runs_dir();
    run_local_in(
        &mut g.run,
        &runs_dir,
        &gate,
        &g.settings,
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
            let env = vec![(DISPATCHED_VAR.to_owned(), "1".to_owned())];
            let before = runs_under(runs_dir, run.id());
            let rec = run.step(&name, &argv, gate, &env)?;
            let inner = runs_under(runs_dir, run.id())
                .into_iter()
                .find(|id| !before.contains(id));
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

/// The runs started under the run `outer` (their `ATELIER_RUNS` list names it): the inner
/// `atelier check` runs of a gate.
pub fn runs_under(runs_dir: &Path, outer: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let Ok(entries) = fs::read_dir(runs_dir) else {
        return found;
    };
    for e in entries.flatten() {
        if let Ok(Some(info)) = RunInfo::read(&e.path())
            && info.id != outer
            && info.runs.iter().any(|r| r == outer)
        {
            found.insert(info.id);
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
    let what = if failed.is_empty() {
        result.failures.join("; ")
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
