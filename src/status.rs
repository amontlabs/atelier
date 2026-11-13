//! `atelier status` (DESIGN §3, §8.1): one read-only view of the preflight and of what the
//! other commands recorded. It converts nothing, takes no exclusive lock and no machine lock,
//! and writes nothing (git runs with `GIT_OPTIONAL_LOCKS=0`; the forecast's objects go to a
//! temporary folder). It exits 0 whatever it finds; only a usage error exits 2.
//!
//! Every section is bounded: at most [`LIST`] entries, then a count of what is left out.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::exit::{Exit, Failure, Result};
use crate::forecast;
use crate::gate;
use crate::git::{self, Checkout};
use crate::ledger::{Approval, MergeLine, Promotion};
use crate::platform::Platform;
use crate::preflight::{self, Context, Report};
use crate::promote::{self, Facts};
use crate::records::{BranchState, MergeFacts};
use crate::runs::{self, Outcome, RunInfo, RunKind, RunResult};
use crate::stack::{Currency, Settlement, Stack, short};
use crate::state::{CURRENT_FORMAT, Versioned};
use crate::testcmd;

/// Entries shown per section.
pub const LIST: usize = 10;

/// Run folders read, newest first. Older ones are counted, not read.
pub const SCAN_RUNS: usize = 300;

/// A named block of lines, and how many entries it leaves out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    pub lines: Vec<String>,
    pub omitted: usize,
}

impl Section {
    fn new(name: impl Into<String>, lines: Vec<String>) -> Section {
        let (lines, omitted) = cap(lines);
        Section {
            name: name.into(),
            lines,
            omitted,
        }
    }
}

fn cap(mut lines: Vec<String>) -> (Vec<String>, usize) {
    let omitted = lines.len().saturating_sub(LIST);
    lines.truncate(LIST);
    (lines, omitted)
}

/// A section that could not be built says so in one line.
fn built(name: impl Into<String>, lines: Result<Vec<String>>) -> Section {
    match lines {
        Ok(l) => Section::new(name, l),
        Err(e) => Section::new(
            name,
            vec![format!(
                "unavailable: {}",
                e.message.lines().next().unwrap_or("")
            )],
        ),
    }
}

/// `atelier status [--json]`.
pub fn run(cwd: &Path, json: bool) -> Result<Exit> {
    git::set_read_only();
    let Report {
        lines,
        state,
        checkout,
        settings,
        pinned_here,
    } = preflight::report(cwd);
    let mut sections = vec![Section {
        name: "preflight".to_owned(),
        lines: lines.iter().flat_map(preflight_lines).collect(),
        omitted: 0,
    }];
    if let (Some(state), Some(checkout), Some(settings)) = (state, checkout, settings) {
        let ctx = Context {
            checkout,
            state,
            settings,
        };
        sections.extend(project(&ctx, pinned_here));
    } else {
        sections.push(Section::new(
            "records",
            vec!["not read: the preflight did not get far enough".to_owned()],
        ));
    }
    if json {
        let value = serde_json::json!({
            "sections": sections.iter().map(|s| serde_json::json!({
                "name": s.name,
                "lines": s.lines,
                "omitted": s.omitted,
            })).collect::<Vec<_>>()
        });
        let text = serde_json::to_string_pretty(&value)
            .map_err(|e| Failure::refused(format!("cannot print the report: {e}")))?;
        println!("{text}");
    } else {
        for s in &sections {
            println!("== {}", s.name);
            if s.lines.is_empty() {
                println!("none");
            }
            for l in &s.lines {
                println!("{l}");
            }
            if s.omitted > 0 {
                println!("... {} more not shown", s.omitted);
            }
        }
    }
    Ok(Exit::Ok)
}

fn preflight_lines(l: &preflight::Line) -> Vec<String> {
    let mut text = l.detail.lines();
    let mut out = vec![format!(
        "{} {}: {}",
        if l.ok { "ok    " } else { "FAILED" },
        l.what,
        text.next().unwrap_or("")
    )];
    out.extend(text.map(|t| format!("  {t}")));
    out
}

/// One run folder as far as it can be read.
struct RunRow {
    id: String,
    result: Option<RunResult>,
    info: Option<RunInfo>,
    dir: PathBuf,
}

impl RunRow {
    /// The platform the run's steps ran on: recorded, or this computer's for a local run.
    fn platform(&self) -> Option<String> {
        let r = self.result.as_ref()?;
        r.platform.clone().or_else(|| {
            r.machine
                .is_none()
                .then(|| Platform::current().map(|p| p.to_string()))
                .flatten()
        })
    }
}

/// The run folders, newest first (their ids start with the UTC minute): at most
/// [`SCAN_RUNS`] read, and how many there are.
fn scan_runs(state: &crate::state::State) -> (Vec<RunRow>, usize) {
    let dir = state.runs_dir();
    let mut ids: Vec<String> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| runs::is_run_id(n))
        .collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    let total = ids.len();
    ids.truncate(SCAN_RUNS);
    let rows = ids
        .into_iter()
        .map(|id| {
            let d = dir.join(&id);
            RunRow {
                result: RunResult::read(&d).ok().flatten(),
                info: RunInfo::read(&d).ok().flatten(),
                id,
                dir: d,
            }
        })
        .collect();
    (rows, total)
}

fn project(ctx: &Context, pinned_here: bool) -> Vec<Section> {
    let root = &ctx.checkout.root;
    let integration = ctx.settings.integration_branch.clone();
    let stack = Stack::new(&ctx.checkout, &ctx.state, &integration);
    let (records, problems) = stack.all_records_lenient();

    let mut versioned_note = None;
    let v = if pinned_here {
        match ctx.state.versioned_view(CURRENT_FORMAT) {
            Ok(v) => v,
            Err(e) => {
                versioned_note = Some(e.message);
                None
            }
        }
    } else {
        versioned_note = Some(
            "the version pinned on the integration branch is not this one: its records are not read"
                .to_owned(),
        );
        None
    };
    let note = || -> Vec<String> {
        versioned_note
            .iter()
            .map(|n| format!("not read: {n}"))
            .collect()
    };

    let (rows, total_runs) = scan_runs(&ctx.state);
    let merges: Vec<MergeLine> = v
        .as_ref()
        .and_then(|v| MergeLine::read_all(v).ok())
        .unwrap_or_default();

    let mut out = vec![built(
        "main",
        main_lines(ctx, &stack, v.as_ref(), versioned_note.is_some()),
    )];
    out.push(match &v {
        Some(v) => built("approvals not yet merged", approvals(root, &stack, v)),
        None => Section::new("approvals not yet merged", note()),
    });
    out.push(match &v {
        Some(_) => built(
            "merges since the last clean",
            Ok(merge_lines(root, &merges, &records)),
        ),
        None => Section::new("merges since the last clean", note()),
    });
    out.push(runs_section(&rows, total_runs));
    out.push(built(
        "platforms",
        Ok(platform_lines(ctx, &stack, &rows, &merges)),
    ));
    out.push(built("machines", testcmd::machine_table(ctx)));
    let mut follow = follow_lines(&stack, &records);
    follow.extend(problems.iter().map(|p| format!("record unreadable: {p}")));
    out.push(Section::new("must follow", follow));
    out.push(built(
        "worktrees with no merge record",
        worktrees(&ctx.checkout, &records),
    ));
    out.push(Section::new("flaky tests", flaky(&rows)));
    out.push(built(
        "conflict forecast",
        forecast_lines(ctx, &stack, &integration),
    ));
    out
}

/// A line's first words: a commit's subject, cut short.
fn subject(root: &Path, oid: &str) -> String {
    let s = git::run(root, ["log", "-1", "--format=%s", oid]).unwrap_or_default();
    match s.char_indices().nth(60) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s,
    }
}

/// `2026-11-13T10:00:07Z` → `2026-11-13T10:00Z`.
fn minute(t: &str) -> String {
    match t.get(..16) {
        Some(m) => format!("{m}Z"),
        None => t.to_owned(),
    }
}

fn main_lines(
    ctx: &Context,
    stack: &Stack,
    v: Option<&Versioned>,
    unread: bool,
) -> Result<Vec<String>> {
    let main_root = gate::main_root(&ctx.checkout);
    let integration = stack.integration();
    let mut out = Vec::new();
    match stack.tip(integration)? {
        Some(tip) => out.push(format!(
            "{integration} at {} {}",
            short(&main_root, &tip),
            subject(&main_root, &tip)
        )),
        None => out.push(format!("{integration}: the branch does not exist")),
    }
    let head = crate::state::MainBranch::read(&ctx.checkout)?.head;
    let changes = promote::tracked_changes(&main_root).unwrap_or_default();
    out.push(format!(
        "main checkout: {}; {}",
        match &head {
            Some(h) if h == integration => format!("on {h}"),
            Some(h) => format!("on {h}, not {integration}"),
            None => "detached HEAD".to_owned(),
        },
        match changes.len() {
            0 => "no tracked changes".to_owned(),
            n => format!("{n} tracked change{}", if n == 1 { "" } else { "s" }),
        }
    ));
    let Some(v) = v else {
        out.push(
            if unread {
                "promotion: not read"
            } else {
                "promotion: none"
            }
            .to_owned(),
        );
        return Ok(out);
    };
    match Promotion::read(v)? {
        None => out.push("promotion: none".to_owned()),
        Some(rec) => out.extend(promotion_lines(&main_root, &rec)?),
    }
    Ok(out)
}

fn promotion_lines(root: &Path, rec: &Promotion) -> Result<Vec<String>> {
    let (old, m) = (short(root, &rec.old), short(root, &rec.m));
    let head = match promote::facts(root, rec)? {
        Facts::Promoted => format!(
            "promotion: {m} is in {} but not recorded (run {}): atelier promote records it",
            rec.integration, rec.run
        ),
        Facts::Moved(_) => format!(
            "promotion: stale: {} moved from {old} (run {}): the next merge or atelier promote --discard drops it",
            rec.integration, rec.run
        ),
        Facts::Pending => {
            if promote::interrupted(root, rec).unwrap_or(false) {
                format!(
                    "promotion: INTERRUPTED, {} {old} -> {m} (run {}): atelier promote lists the paths; atelier promote --discard drops it",
                    rec.integration, rec.run
                )
            } else {
                format!(
                    "promotion: pending, {} {old} -> {m} (run {}): atelier promote",
                    rec.integration, rec.run
                )
            }
        }
    };
    let mut out = vec![head];
    out.push(format!(
        "  branches: {}",
        rec.branches
            .iter()
            .map(|b| b.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    if let Some(t) = &rec.push_started {
        out.push(format!(
            "  push started {}, {}",
            minute(t),
            match &rec.push_ended {
                Some(e) => format!("ended {}", minute(e)),
                None => "not ended".to_owned(),
            }
        ));
    }
    Ok(out)
}

fn approvals(root: &Path, stack: &Stack, v: &Versioned) -> Result<Vec<String>> {
    let (list, problems) = Approval::list(v);
    let mut out = Vec::new();
    for a in &list {
        let tip = stack.tip(&a.branch).ok().flatten();
        let note = match tip {
            None => " (the branch is gone)".to_owned(),
            Some(t) if t != a.commit => format!(" (the branch is now at {})", short(root, &t)),
            Some(_) => String::new(),
        };
        out.push(format!(
            "{} {} approved {}{note}",
            a.branch,
            short(root, &a.commit),
            minute(&a.approved)
        ));
    }
    out.extend(problems.iter().map(|p| format!("unreadable: {p}")));
    Ok(out)
}

/// Merges whose branch record is still `merged`: `clean` has not deleted them yet.
fn merge_lines(
    root: &Path,
    merges: &[MergeLine],
    records: &[crate::records::BranchRecord],
) -> Vec<String> {
    let mut out = Vec::new();
    for l in merges.iter().rev() {
        let pending = records.iter().any(|r| {
            r.name == l.branch && r.lifetime == l.lifetime && r.state == BranchState::Merged
        });
        if pending {
            out.push(format!(
                "{} {} {} run {}",
                minute(&l.time),
                l.branch,
                short(root, &l.commit),
                l.run
            ));
        }
    }
    out
}

fn runs_section(rows: &[RunRow], total: usize) -> Section {
    let mut lines = Vec::new();
    for r in rows.iter().take(LIST) {
        lines.push(run_line(r));
    }
    let omitted = total.saturating_sub(lines.len());
    Section {
        name: "last runs".to_owned(),
        lines,
        omitted,
    }
}

fn run_line(r: &RunRow) -> String {
    let Some(res) = &r.result else {
        let live = r.info.as_ref().is_some_and(|i| {
            i.pid_start.is_some()
                && crate::procs::start_time(i.pid as i32).as_deref() == i.pid_start.as_deref()
        });
        let kind = r.info.as_ref().map(|i| i.kind.as_str()).unwrap_or("?");
        return format!(
            "{} {kind} {}",
            r.id,
            if live {
                "RUNNING"
            } else {
                "NO RESULT (killed)"
            }
        );
    };
    let word = match res.outcome {
        Outcome::Passed => "PASS",
        Outcome::Failed => "FAIL",
        Outcome::Interrupted => "INTERRUPTED",
    };
    let mut line = format!("{} {} {word}", r.id, res.kind.as_str());
    if let Some(b) = &res.branch {
        line.push_str(&format!(" {b}"));
    }
    if let Some(p) = r.platform() {
        line.push_str(&format!(" {p}"));
    }
    if let Some(m) = &res.machine {
        line.push_str(&format!(" on {m}"));
    }
    if let Some(t) = &res.tree {
        line.push_str(&format!(" tree {}", &t[..t.len().min(12)]));
    }
    line
}

/// `platform=pass@machine` or `platform=waived:reason` of a merge line.
fn merge_entry<'a>(l: &'a MergeLine, platform: &str) -> Option<&'a str> {
    l.platforms.iter().find_map(|p| {
        let (name, rest) = p.split_once('=')?;
        (name == platform).then_some(rest)
    })
}

fn platform_lines(
    ctx: &Context,
    stack: &Stack,
    rows: &[RunRow],
    merges: &[MergeLine],
) -> Vec<String> {
    let root = &ctx.checkout.root;
    let committed = stack
        .tip(stack.integration())
        .ok()
        .flatten()
        .and_then(|tip| gate::settings_at(root, &tip).ok().flatten());
    let names: Vec<String> = committed
        .as_ref()
        .unwrap_or(&ctx.settings)
        .platforms
        .keys()
        .cloned()
        .collect();
    let mut out = Vec::new();
    for p in names {
        // (time, is a pass, text)
        let mut seen: Vec<(String, bool, String)> = Vec::new();
        for r in rows {
            let Some(res) = &r.result else { continue };
            let full = matches!(res.kind, RunKind::Check | RunKind::TestFull | RunKind::Gate);
            if !full || res.outcome == Outcome::Interrupted || r.platform().as_deref() != Some(&p) {
                continue;
            }
            let pass = res.outcome == Outcome::Passed;
            seen.push((
                res.ended.clone(),
                pass,
                format!(
                    "{} {} on {} ({})",
                    if pass { "PASS" } else { "FAIL" },
                    minute(&res.ended),
                    res.machine.as_deref().unwrap_or("this computer"),
                    res.kind.as_str()
                ),
            ));
        }
        for l in merges {
            match merge_entry(l, &p) {
                Some(e) if e.starts_with("pass@") => seen.push((
                    l.time.clone(),
                    true,
                    format!(
                        "PASS {} on {} (merge of {})",
                        minute(&l.time),
                        match &e[5..] {
                            "local" => "this computer",
                            m => m,
                        },
                        l.branch
                    ),
                )),
                _ => {}
            }
        }
        seen.sort();
        let last_pass = seen.iter().rfind(|s| s.1).map(|s| s.0.clone());
        let mut line = match seen.last() {
            Some((_, _, text)) => format!("{p}: {text}"),
            None => format!("{p}: no run recorded"),
        };
        let waived: Vec<&MergeLine> = merges
            .iter()
            .filter(|l| {
                merge_entry(l, &p).is_some_and(|e| e.starts_with("waived:"))
                    && last_pass.as_ref().is_none_or(|t| l.time > *t)
            })
            .collect();
        if let Some(last) = waived.last() {
            let reason = merge_entry(last, &p)
                .and_then(|e| e.strip_prefix("waived:"))
                .unwrap_or("");
            line.push_str(&format!(
                "; waived since: {} (last {} {}: {reason})",
                waived.len(),
                minute(&last.time),
                last.branch
            ));
        }
        out.push(line);
    }
    out
}

/// Branches that must `atelier follow`, and starts that did not finish.
fn follow_lines(stack: &Stack, records: &[crate::records::BranchRecord]) -> Vec<String> {
    let mut out = Vec::new();
    for rec in records.iter().filter(|r| r.state == BranchState::Active) {
        if rec.starting.is_some() {
            out.push(format!(
                "{}: its start did not finish: run atelier start {} again",
                rec.name, rec.name
            ));
            continue;
        }
        if let (Some(_), Ok(Some(tip))) = (&rec.pending, stack.tip(&rec.name)) {
            match stack.settle_view(rec, &tip) {
                Ok(Settlement::InProgress(wt)) => out.push(format!(
                    "{}: a follow is in progress in {}",
                    rec.name,
                    wt.display()
                )),
                Ok(Settlement::Unknown { .. }) => out.push(format!(
                    "{}: cannot tell whether its follow finished or was aborted",
                    rec.name
                )),
                _ => {}
            }
        }
        match stack.currency(&rec.name) {
            Ok(Currency::Current) => {}
            Ok(Currency::Behind(b)) => out.push(b.message()),
            Err(e) => out.push(format!("{}: {}", rec.name, e.message)),
        }
    }
    let mut seen = BTreeSet::new();
    out.retain(|l| seen.insert(l.clone()));
    out
}

/// Linked worktrees whose branch has no merge record that names them: `clean` never touches
/// them.
fn worktrees(checkout: &Checkout, records: &[crate::records::BranchRecord]) -> Result<Vec<String>> {
    let gate = git::canonical(&gate::gate_worktree(checkout));
    let mut out = Vec::new();
    for w in git::worktree_list(&checkout.root)? {
        if w.main || git::canonical(&w.path) == gate {
            continue;
        }
        let branch = w
            .branch
            .as_deref()
            .map(|b| b.strip_prefix("refs/heads/").unwrap_or(b).to_owned());
        let rec = branch
            .as_ref()
            .and_then(|b| records.iter().find(|r| &r.name == b));
        let facts = rec
            .filter(|r| r.state == BranchState::Merged)
            .and_then(|r| MergeFacts::read(r).ok().flatten());
        if let Some(f) = &facts
            && f.worktree == git::canonical(&w.path)
        {
            continue;
        }
        let why = match (&branch, rec) {
            (None, _) => "detached".to_owned(),
            (Some(_), None) => "no record: not started by atelier".to_owned(),
            (Some(_), Some(r)) if r.state == BranchState::Active => "active".to_owned(),
            (Some(_), Some(_)) if facts.is_some() => "merged from another worktree".to_owned(),
            (Some(_), Some(_)) => "no merge facts".to_owned(),
        };
        out.push(format!(
            "{} {} ({why}{})",
            w.path.display(),
            branch.as_deref().unwrap_or("-"),
            if w.prunable { ", folder missing" } else { "" }
        ));
    }
    Ok(out)
}

/// Tests that failed, then passed, on the same tree and platform in full runs (`check`, a
/// platform's whole `run`, the gate's), from the per-test results. Narrow runs and compile-only
/// runs never count.
fn flaky(rows: &[RunRow]) -> Vec<String> {
    type Key = (String, String, String, String);
    // verdict (true = pass) in run order, with the run
    let mut seen: BTreeMap<Key, Vec<(bool, String)>> = BTreeMap::new();
    let mut ordered: Vec<&RunRow> = rows.iter().collect();
    ordered.sort_by(|a, b| {
        let key = |r: &RunRow| (r.result.as_ref().map(|x| x.started.clone()), r.id.clone());
        key(a).cmp(&key(b))
    });
    for r in ordered {
        let Some(res) = &r.result else { continue };
        if !matches!(res.kind, RunKind::Check | RunKind::TestFull | RunKind::Gate)
            || res.outcome == Outcome::Interrupted
        {
            continue;
        }
        let (Some(tree), Some(platform), Some(rel)) = (&res.tree, r.platform(), &res.test_results)
        else {
            continue;
        };
        let rel = Path::new(rel);
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            continue;
        }
        let Ok(text) = fs::read_to_string(r.dir.join(rel)) else {
            continue;
        };
        for line in text.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            let pass = match f.get(2).copied() {
                Some("pass") => true,
                Some("fail") => false,
                _ => continue,
            };
            let key = (
                tree.clone(),
                platform.clone(),
                f[0].to_owned(),
                f[1].to_owned(),
            );
            seen.entry(key).or_default().push((pass, r.id.clone()));
        }
    }
    let mut found: Vec<(String, String)> = Vec::new();
    for ((tree, platform, ws, test), verdicts) in &seen {
        let Some(first_fail) = verdicts.iter().position(|v| !v.0) else {
            continue;
        };
        let Some(pass) = verdicts[first_fail..].iter().find(|v| v.0) else {
            continue;
        };
        found.push((
            pass.1.clone(),
            format!(
                "{ws} {test}: failed in {}, passed in {} ({platform}, tree {})",
                verdicts[first_fail].1,
                pass.1,
                &tree[..tree.len().min(12)]
            ),
        ));
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|f| f.1).collect()
}

fn forecast_lines(ctx: &Context, stack: &Stack, integration: &str) -> Result<Vec<String>> {
    let Some(tip) = stack.tip(integration)? else {
        return Ok(vec![format!("{integration}: the branch does not exist")]);
    };
    let open = forecast::open_branches(stack);
    let regen: BTreeSet<String> = gate::settings_at(&ctx.checkout.root, &tip)?
        .map(|s| gate::regenerated_paths(&s))
        .unwrap_or_default();
    let main = forecast::Open {
        name: integration.to_owned(),
        tip,
    };
    let f = forecast::forecast(&ctx.checkout, &main, &open, &regen);
    let mut out: Vec<String> = f.conflicts.iter().map(|c| c.line(5)).collect();
    if out.is_empty() {
        out.push(format!(
            "no conflicts among {} open branch{} and {integration}",
            open.len(),
            if open.len() == 1 { "" } else { "es" }
        ));
    }
    if f.unknown > 0 {
        out.push(format!("{} pairs not forecast", f.unknown));
    }
    Ok(out)
}
