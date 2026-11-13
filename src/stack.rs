//! Stacked branches: the branch record lifecycle and the ancestry rules every
//! command that reads a record applies.
//!
//! A branch atelier started has one record (`records::BranchRecord`, frozen layer) naming its
//! lower branch (by name and lifetime id) and its base, the exact commit it is built on. From
//! those facts and git's ancestry alone, this module decides:
//!
//! - how a pending `follow` ended ([`Stack::settle_view`]): finished, aborted, or not decidable;
//! - whether the base is still in the branch ([`Stack::check_base`]); there is no automatic
//!   repair: ancestry cannot prove which commits are the worker's own;
//! - whether a rebase of a branch is open in any worktree of the clone ([`Stack::rebase_open`]),
//!   whether or not a `follow` started it;
//! - what `follow` rebases onto ([`Stack::target`]);
//! - whether a branch is *current*, transitively up to the integration branch
//!   ([`Stack::currency`]), which `approve` and `merge` require;
//! - which records `clean` must keep ([`Stack::dependents`]).
//!
//! The pure functions never write. Writers ([`Stack::settle`], [`Stack::prepare`],
//! [`Stack::set_state`], [`Stack::drop_if_unneeded`]) expect the caller to hold the branch's
//! lock (`locks::LockKey::branch`).

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::exit::{Failure, IoContext, Result};
use crate::git::{self, Checkout};
use crate::names;
use crate::records::{BranchRecord, BranchState};
use crate::refs::{self, check_oid};
use crate::state::State;

/// The refusal for a branch without an active record.
pub fn not_started(branch: &str) -> Failure {
    Failure::refused(format!(
        "{branch} was not started by atelier: start it with atelier start"
    ))
}

/// The refusal while a rebase is open.
pub const FINISH_REBASE: &str = "finish the rebase: git rebase --continue or --abort";

/// Git facts and the records of one clone, for the stacking rules.
#[derive(Debug)]
pub struct Stack<'s> {
    /// A folder of the clone (any worktree): where git runs.
    dir: PathBuf,
    common_dir: PathBuf,
    state: &'s State,
    integration: String,
}

/// How a pending `follow` ended, decided from ancestry alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    /// No pending follow.
    None,
    /// The rebase finished: the base becomes the target.
    Finished { target: String },
    /// The rebase was aborted: pending is dropped, the base stays.
    Aborted,
    /// A rebase of this branch is open in the given worktree folder.
    InProgress(PathBuf),
    /// Neither: both commits are named in the refusal.
    Unknown { target: String, old_base: String },
}

/// What `follow` rebases onto.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub commit: String,
    /// True when the target is the integration branch because the lower branch was merged, or
    /// is gone and the integration branch contains the base: the record then names the
    /// integration branch as its lower.
    pub repoint: bool,
}

/// Why a branch is not current: the first branch of its chain that must `atelier follow`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Behind {
    pub branch: String,
    pub why: Why,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// Its base is not in its lower branch's tip (or in the integration branch, once the lower
    /// branch is merged).
    LowerRewritten { lower: String },
    /// Its lower branch's ref is gone and the lower was never merged.
    LowerGone { lower: String },
    /// Its lower branch's record is missing or belongs to another lifetime.
    LowerRestarted { lower: String },
    /// Its base is no longer in it.
    BaseLost { base: String },
    /// A rebase of it is open.
    RebaseOpen,
    /// Its pending follow cannot be settled.
    Unsettled { target: String, old_base: String },
    /// No active record, or no branch ref.
    NotStarted,
    /// Its `start` did not finish.
    StartUnfinished,
}

impl Behind {
    /// The refusal text `approve` and `merge` print.
    pub fn message(&self) -> String {
        let b = &self.branch;
        match &self.why {
            Why::LowerRewritten { lower } => {
                format!("{b} is behind a rewrite of {lower}: run atelier follow in its worktree")
            }
            Why::LowerGone { lower } => format!(
                "{b} is built on {lower}, which is gone and was never merged: run atelier follow in its worktree"
            ),
            Why::LowerRestarted { lower } => format!(
                "{b} is built on an earlier {lower}, whose record is gone: start {b} again on what it needs"
            ),
            Why::BaseLost { base } => format!("{b}: {}", base_lost(base).message),
            Why::RebaseOpen => format!("{b} has a rebase open: {FINISH_REBASE}"),
            Why::Unsettled { target, old_base } => format!(
                "{b}: cannot tell whether its follow onto {target} (from {old_base}) finished or was aborted"
            ),
            Why::NotStarted => not_started(b).message,
            Why::StartUnfinished => start_unfinished(b).message,
        }
    }
}

/// Whether a branch is current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Currency {
    Current,
    Behind(Behind),
}

/// One branch of a chain, from [`Stack::chain`].
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    /// The record as every reading command sees it: pending settled (not written).
    pub record: BranchRecord,
    /// The branch's tip.
    pub tip: String,
}

impl<'s> Stack<'s> {
    /// `integration` is the integration branch's name (from the settings).
    pub fn new(checkout: &Checkout, state: &'s State, integration: &str) -> Stack<'s> {
        Stack {
            dir: checkout.root.clone(),
            common_dir: checkout.common_dir.clone(),
            state,
            integration: integration.to_owned(),
        }
    }

    pub fn integration(&self) -> &str {
        &self.integration
    }

    pub fn state(&self) -> &State {
        self.state
    }

    /// A branch's tip; `None` when the branch does not exist. The ref must be plain.
    pub fn tip(&self, branch: &str) -> Result<Option<String>> {
        let tip = refs::read(&self.dir, &names::branch_ref(branch))?;
        match tip {
            Some(t) => Ok(Some(commit_of(&self.dir, &t)?)),
            None => Ok(None),
        }
    }

    /// The integration branch's tip; refuses (4) when it does not exist.
    pub fn integration_tip(&self) -> Result<String> {
        self.tip(&self.integration)?.ok_or_else(|| {
            Failure::refused(format!(
                "the integration branch {} does not exist",
                self.integration
            ))
        })
    }

    /// A branch's record, whatever its state.
    pub fn record(&self, branch: &str) -> Result<Option<BranchRecord>> {
        BranchRecord::read(self.state, branch)
    }

    /// The active record of a branch that exists and whose `start` finished; refuses (4)
    /// otherwise. Returns the record and the tip.
    pub fn started(&self, branch: &str) -> Result<(BranchRecord, String)> {
        let rec = self.record(branch)?;
        let Some(rec) = rec.filter(|r| r.state == BranchState::Active) else {
            return Err(not_started(branch));
        };
        if rec.starting.is_some() {
            return Err(start_unfinished(branch));
        }
        let tip = self.tip(branch)?.ok_or_else(|| not_started(branch))?;
        Ok((rec, tip))
    }

    /// Every branch record, sorted by name. Refuses (4) one that cannot be read.
    pub fn all_records(&self) -> Result<Vec<BranchRecord>> {
        let dir = self.state.root().join("branches");
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Failure::refused(format!("{}: {e}", dir.display()))),
        };
        let mut out = Vec::new();
        for entry in entries {
            let path = entry.ctx(dir.display())?.path();
            let Some(stem) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".toml"))
            else {
                continue;
            };
            let Some(name) = names::unkey(stem) else {
                continue;
            };
            if let Some(r) = BranchRecord::read(self.state, &name)? {
                out.push(r);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Every branch record that can be read, sorted by name, and a line for each that cannot.
    /// For `status`, which reports what it finds instead of refusing.
    pub fn all_records_lenient(&self) -> (Vec<BranchRecord>, Vec<String>) {
        let dir = self.state.root().join("branches");
        let mut out = Vec::new();
        let mut problems = Vec::new();
        let Ok(entries) = fs::read_dir(&dir) else {
            return (out, problems);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".toml"))
                .and_then(names::unkey)
            else {
                continue;
            };
            match BranchRecord::read(self.state, &name) {
                Ok(Some(r)) => out.push(r),
                Ok(None) => {}
                Err(e) => problems.push(e.message),
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        problems.sort();
        (out, problems)
    }

    /// The active branches whose record names `lower` with this lifetime as their lower
    /// branch, unfinished starts included: while any exists, `clean` keeps `lower`'s record and
    /// `start` refuses its name.
    pub fn dependents(&self, lower: &str, lifetime: &str) -> Result<Vec<String>> {
        Ok(self
            .all_records()?
            .into_iter()
            .filter(|r| {
                r.state == BranchState::Active
                    && !r.lower_is_integration
                    && r.lower == lower
                    && r.lower_lifetime.as_deref() == Some(lifetime)
            })
            .map(|r| r.name)
            .collect())
    }

    /// Sets a branch's state (`merge` → merged, `clean` → cleaned), only for the given
    /// lifetime. Refuses (4) when the record is missing or belongs to another lifetime. Fields
    /// this version does not know are kept.
    pub fn set_state(&self, branch: &str, lifetime: &str, to: BranchState) -> Result<()> {
        let mut rec = self
            .record(branch)?
            .filter(|r| r.lifetime == lifetime)
            .ok_or_else(|| {
                Failure::refused(format!(
                    "{branch}: no record of lifetime {lifetime}; it was started again"
                ))
            })?;
        rec.state = to;
        rec.write(self.state)
    }

    /// Removes a cleaned branch's record of this lifetime once no active record names it as
    /// its lower branch. Returns whether it was removed; `false` names nothing to do or a
    /// record still kept for a dependent.
    pub fn drop_if_unneeded(&self, branch: &str, lifetime: &str) -> Result<bool> {
        let Some(rec) = self.record(branch)? else {
            return Ok(false);
        };
        if rec.lifetime != lifetime
            || rec.state != BranchState::Cleaned
            || !self.dependents(branch, lifetime)?.is_empty()
        {
            return Ok(false);
        }
        let path = self.state.branch_record_path(branch);
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(Failure::refused(format!("{}: {e}", path.display()))),
        }
    }

    /// The worktree folder in which a rebase of `branch` is open, if any: the git folder of
    /// some worktree of the clone names the branch in `rebase-merge/head-name` or
    /// `rebase-apply/head-name`. A hand rebase counts as much as one `follow` started.
    pub fn rebase_open(&self, branch: &str) -> Result<Option<PathBuf>> {
        let want = names::branch_ref(branch);
        for gd in self.git_dirs()? {
            for sub in ["rebase-merge", "rebase-apply"] {
                if let Ok(name) = fs::read_to_string(gd.join(sub).join("head-name"))
                    && name.trim_end() == want
                {
                    return Ok(Some(worktree_of(&gd)));
                }
            }
        }
        Ok(None)
    }

    /// The worktree folder in which a bisect started on `branch` is open, if any: the git folder
    /// of some worktree of the clone names the branch in `BISECT_START` (as git counts a branch
    /// in use by a bisect).
    pub fn bisect_open(&self, branch: &str) -> Result<Option<PathBuf>> {
        let full = names::branch_ref(branch);
        for gd in self.git_dirs()? {
            if let Ok(name) = fs::read_to_string(gd.join("BISECT_START")) {
                let name = name.trim_end();
                if name == branch || name == full {
                    return Ok(Some(worktree_of(&gd)));
                }
            }
        }
        Ok(None)
    }

    /// The git folder of every worktree of the clone: the common folder (the main checkout's)
    /// and each linked worktree's.
    fn git_dirs(&self) -> Result<Vec<PathBuf>> {
        let mut git_dirs = vec![self.common_dir.clone()];
        match fs::read_dir(self.common_dir.join("worktrees")) {
            Ok(entries) => {
                for e in entries.flatten() {
                    git_dirs.push(e.path());
                }
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(Failure::refused(format!("worktrees: {e}"))),
        }
        Ok(git_dirs)
    }

    /// Refuses (4) while a rebase of `branch` is open in any worktree of the clone.
    pub fn refuse_rebase_open(&self, branch: &str) -> Result<()> {
        match self.rebase_open(branch)? {
            None => Ok(()),
            Some(wt) => Err(Failure::refused(format!(
                "{branch}: a rebase is open in {}: {FINISH_REBASE}",
                wt.display()
            ))),
        }
    }

    /// How a branch's pending follow ended, from ancestry alone. Pure.
    pub fn settle_view(&self, rec: &BranchRecord, tip: &str) -> Result<Settlement> {
        let Some(p) = &rec.pending else {
            return Ok(Settlement::None);
        };
        if let Some(wt) = self.rebase_open(&rec.name)? {
            return Ok(Settlement::InProgress(wt));
        }
        let target_in = is_ancestor(&self.dir, &p.target, tip)?;
        let old_in = is_ancestor(&self.dir, &p.old_base, tip)?;
        if target_in && (!old_in || is_ancestor(&self.dir, &p.old_base, &p.target)?) {
            return Ok(Settlement::Finished {
                target: p.target.clone(),
            });
        }
        if old_in {
            return Ok(Settlement::Aborted);
        }
        Ok(Settlement::Unknown {
            target: p.target.clone(),
            old_base: p.old_base.clone(),
        })
    }

    /// Settles a pending follow and writes the record. Refuses (4) while a rebase is open and
    /// when the outcome cannot be told. The caller holds the branch lock.
    pub fn settle(&self, mut rec: BranchRecord, tip: &str) -> Result<BranchRecord> {
        match self.settle_view(&rec, tip)? {
            Settlement::None => return Ok(rec),
            Settlement::Finished { target } => rec.base = target,
            Settlement::Aborted => {}
            Settlement::InProgress(wt) => {
                return Err(Failure::refused(format!(
                    "{}: a rebase is open in {}: {FINISH_REBASE}",
                    rec.name,
                    wt.display()
                )));
            }
            Settlement::Unknown { target, old_base } => {
                return Err(Failure::refused(format!(
                    "{}: cannot tell whether the follow onto {target} (from {old_base}) finished or was aborted: neither commit is where a finished or an aborted rebase leaves it",
                    rec.name
                )));
            }
        }
        rec.pending = None;
        rec.write(self.state)?;
        Ok(rec)
    }

    /// The lower branch's tip as `follow` uses it, or why there is none. Pure.
    ///
    /// - lower is the integration branch: its tip;
    /// - lower merged (or cleaned) by its record: the integration branch's tip, `repoint`;
    /// - lower active and present: its tip;
    /// - lower gone (no ref, or no record of the recorded lifetime): the integration branch's
    ///   tip when it contains the base, `repoint`; otherwise refuse (4).
    pub fn target(&self, rec: &BranchRecord) -> Result<Target> {
        if rec.lower_is_integration {
            return Ok(Target {
                commit: self.integration_tip()?,
                repoint: false,
            });
        }
        let lower = self
            .record(&rec.lower)?
            .filter(|l| Some(&l.lifetime) == rec.lower_lifetime.as_ref());
        if let Some(l) = &lower {
            match l.state {
                BranchState::Merged | BranchState::Cleaned => {
                    return Ok(Target {
                        commit: self.integration_tip()?,
                        repoint: true,
                    });
                }
                BranchState::Active => {
                    if let Some(tip) = self.tip(&rec.lower)? {
                        return Ok(Target {
                            commit: tip,
                            repoint: false,
                        });
                    }
                }
            }
        }
        let main = self.integration_tip()?;
        if is_ancestor(&self.dir, &rec.base, &main)? {
            return Ok(Target {
                commit: main,
                repoint: true,
            });
        }
        Err(Failure::refused(format!(
            "{} is gone and was never merged: {} stays as it is; restore {}, or start {} again on what it needs",
            rec.lower, rec.name, rec.lower, rec.name
        )))
    }

    /// The base check every command that reads a record makes. Pure.
    /// Refuses (4) a base that is no longer in the branch. There is no repair: a lower tip in
    /// the branch also holds after a wrong hand rebase that replayed commits the lower branch
    /// dropped (3.3 review 9).
    pub fn check_base(&self, rec: &BranchRecord, tip: &str) -> Result<()> {
        if is_ancestor(&self.dir, &rec.base, tip)? {
            Ok(())
        } else {
            Err(base_lost(&rec.base))
        }
    }

    /// What every command that reads a branch's record does first: requires an active record
    /// of a finished start and the branch, refuses a rebase of it open in any worktree, settles
    /// a pending follow (writing the record) and checks the base. Returns the record and the
    /// tip. The caller holds the branch lock.
    pub fn prepare(&self, branch: &str) -> Result<(BranchRecord, String)> {
        let (rec, tip) = self.started(branch)?;
        self.refuse_rebase_open(branch)?;
        let rec = self.settle(rec, &tip)?;
        self.check_base(&rec, &tip)?;
        Ok((rec, tip))
    }

    /// The record as every reading command would leave it, without writing: pending settled.
    /// A branch that cannot be read that way (not started, start unfinished, a rebase open in
    /// any worktree, pending not settleable, base lost) is returned as [`Behind`].
    fn view(&self, branch: &str) -> Result<std::result::Result<Link, Behind>> {
        let behind = |why| Behind {
            branch: branch.to_owned(),
            why,
        };
        let rec = self
            .record(branch)?
            .filter(|r| r.state == BranchState::Active);
        let (Some(mut rec), Some(tip)) = (rec, self.tip(branch)?) else {
            return Ok(Err(behind(Why::NotStarted)));
        };
        if rec.starting.is_some() {
            return Ok(Err(behind(Why::StartUnfinished)));
        }
        if self.rebase_open(branch)?.is_some() {
            return Ok(Err(behind(Why::RebaseOpen)));
        }
        match self.settle_view(&rec, &tip)? {
            Settlement::None | Settlement::Aborted => {}
            Settlement::Finished { target } => rec.base = target,
            Settlement::InProgress(_) => return Ok(Err(behind(Why::RebaseOpen))),
            Settlement::Unknown { target, old_base } => {
                return Ok(Err(behind(Why::Unsettled { target, old_base })));
            }
        }
        rec.pending = None;
        if !is_ancestor(&self.dir, &rec.base, &tip)? {
            return Ok(Err(behind(Why::BaseLost {
                base: rec.base.clone(),
            })));
        }
        Ok(Ok(Link { record: rec, tip }))
    }

    /// The branch and its unmerged lower branches, from the branch down to (not including)
    /// the integration branch or the first merged lower branch. Each record is as every
    /// reading command sees it (pending settled; nothing written). Refuses (4)
    /// a branch of the chain that is not readable that way, and records that form a cycle.
    ///
    /// `merge` uses it for the rule that a lower branch lands only when approved: every
    /// `chain[i + 1]` must be in the same batch, earlier, approved at a commit that contains
    /// `chain[i].record.base`. Call [`Stack::require_current`] first: the chain also ends at a
    /// lower branch whose record is missing or of another lifetime, which only that refuses.
    pub fn chain(&self, branch: &str) -> Result<Vec<Link>> {
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        let mut name = branch.to_owned();
        loop {
            if !seen.insert(name.clone()) {
                return Err(cycle(&out, &name));
            }
            let link = self
                .view(&name)?
                .map_err(|b| Failure::refused(b.message()))?;
            let rec = &link.record;
            let next = (!rec.lower_is_integration).then(|| rec.lower.clone());
            let lower_lifetime = rec.lower_lifetime.clone();
            out.push(link);
            let Some(next) = next else { break };
            match self.record(&next)? {
                Some(l)
                    if Some(&l.lifetime) == lower_lifetime.as_ref()
                        && l.state == BranchState::Active => {}
                _ => break,
            }
            name = next;
        }
        Ok(out)
    }

    /// Whether a branch is current: its base is in its lower branch's tip (or in
    /// the integration branch once the lower is merged), and its lower branch is current, up
    /// to the integration branch. When it is not, names the branch of its chain closest to the
    /// integration branch that must `atelier follow`. Pure. Refuses (4) records that form a
    /// cycle.
    pub fn currency(&self, branch: &str) -> Result<Currency> {
        let mut deepest = None;
        let mut seen = BTreeSet::new();
        let mut name = branch.to_owned();
        let mut path: Vec<String> = Vec::new();
        loop {
            if !seen.insert(name.clone()) {
                path.push(name);
                return Err(Failure::refused(format!(
                    "branch records form a cycle: {}",
                    path.join(" → ")
                )));
            }
            path.push(name.clone());
            let link = match self.view(&name)? {
                Ok(l) => l,
                Err(b) => {
                    deepest = Some(b);
                    break;
                }
            };
            let rec = link.record;
            let behind = |why| Behind {
                branch: name.clone(),
                why,
            };
            if rec.lower_is_integration {
                if !is_ancestor(&self.dir, &rec.base, &self.integration_tip()?)? {
                    deepest = Some(behind(Why::LowerRewritten {
                        lower: self.integration.clone(),
                    }));
                }
                break;
            }
            let lower = self
                .record(&rec.lower)?
                .filter(|l| Some(&l.lifetime) == rec.lower_lifetime.as_ref());
            let Some(lower) = lower else {
                deepest = Some(behind(Why::LowerRestarted {
                    lower: rec.lower.clone(),
                }));
                break;
            };
            if lower.state != BranchState::Active {
                if !is_ancestor(&self.dir, &rec.base, &self.integration_tip()?)? {
                    deepest = Some(behind(Why::LowerRewritten {
                        lower: rec.lower.clone(),
                    }));
                }
                break;
            }
            let Some(lower_tip) = self.tip(&rec.lower)? else {
                deepest = Some(behind(Why::LowerGone {
                    lower: rec.lower.clone(),
                }));
                break;
            };
            if !is_ancestor(&self.dir, &rec.base, &lower_tip)? {
                deepest = Some(behind(Why::LowerRewritten {
                    lower: rec.lower.clone(),
                }));
            }
            name = rec.lower;
        }
        Ok(match deepest {
            None => Currency::Current,
            Some(b) => Currency::Behind(b),
        })
    }

    /// Refuses (4) a branch that is not current, with [`Behind::message`].
    pub fn require_current(&self, branch: &str) -> Result<()> {
        match self.currency(branch)? {
            Currency::Current => Ok(()),
            Currency::Behind(b) => Err(Failure::refused(b.message())),
        }
    }

    /// The merge commits among the branch's own commits: in `tip`, in neither its base nor
    /// its lower tip. `approve`, `merge` and `follow` refuse a branch that has any.
    pub fn own_merges(&self, rec: &BranchRecord, tip: &str) -> Result<Vec<String>> {
        let mut args = vec![
            "rev-list".to_owned(),
            "--merges".to_owned(),
            tip.to_owned(),
            format!("^{}", rec.base),
        ];
        if let Ok(t) = self.target(rec) {
            args.push(format!("^{}", t.commit));
        }
        let text = git::run(&self.dir, &args)?;
        Ok(text.lines().map(str::to_owned).collect())
    }

    /// Checks a new branch's lower branch for `start`: the name is not the branch itself and
    /// no chain below it reaches the branch (a cycle). Refuses (4).
    pub fn check_no_cycle(&self, branch: &str, lower: &str) -> Result<()> {
        if branch == lower {
            return Err(Failure::refused(format!(
                "{branch} cannot be built on itself"
            )));
        }
        let mut seen = BTreeSet::new();
        let mut path = vec![branch.to_owned()];
        let mut name = lower.to_owned();
        while name != self.integration {
            path.push(name.clone());
            if name == branch || !seen.insert(name.clone()) {
                return Err(Failure::refused(format!(
                    "--on {lower} would make a cycle: {}",
                    path.join(" → ")
                )));
            }
            let Some(rec) = self.record(&name)? else {
                break;
            };
            if rec.lower_is_integration || rec.state != BranchState::Active {
                break;
            }
            name = rec.lower;
        }
        Ok(())
    }
}

fn cycle(out: &[Link], again: &str) -> Failure {
    let mut path: Vec<&str> = out.iter().map(|l| l.record.name.as_str()).collect();
    path.push(again);
    Failure::refused(format!("branch records form a cycle: {}", path.join(" → ")))
}

/// The refusal for a base the branch no longer contains (word for word).
pub fn base_lost(base: &str) -> Failure {
    Failure::refused(format!(
        "your branch no longer contains its base {base}: restore it (git reflog, then git reset --hard <the commit before your rebase>) and use atelier follow"
    ))
}

/// The refusal for a branch whose `start` did not finish.
pub fn start_unfinished(branch: &str) -> Failure {
    Failure::refused(format!(
        "the start of {branch} did not finish: run atelier start {branch} again to finish or undo it"
    ))
}

/// The worktree folder of a git folder: the main checkout's for the common folder, the one
/// `gitdir` names for a linked worktree's.
fn worktree_of(git_dir: &Path) -> PathBuf {
    if let Ok(text) = fs::read_to_string(git_dir.join("gitdir")) {
        let p = PathBuf::from(text.trim_end());
        if let Some(parent) = p.parent() {
            return parent.to_path_buf();
        }
    }
    git_dir.parent().unwrap_or(git_dir).to_path_buf()
}

/// The commit an object id names (peeling a tag); refuses (4) one that is not a commit.
pub fn commit_of(dir: &Path, oid: &str) -> Result<String> {
    let out = git::output(
        dir,
        ["rev-parse", "-q", "--verify", &format!("{oid}^{{commit}}")],
    )?;
    if !out.status.success() {
        return Err(Failure::refused(format!("{oid} is not a commit")));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// True when commit `a` is an ancestor of (or equal to) `b`. A commit missing from the
/// repository (collected after a rebase) is in nothing. Both must be full object ids.
pub fn is_ancestor(dir: &Path, a: &str, b: &str) -> Result<bool> {
    check_oid(a).map_err(|f| Failure::refused(f.message))?;
    check_oid(b).map_err(|f| Failure::refused(f.message))?;
    let exists = git::output(dir, ["cat-file", "-e", &format!("{a}^{{commit}}")])?;
    if !exists.status.success() {
        return Ok(false);
    }
    let out = git::output(dir, ["merge-base", "--is-ancestor", a, b])?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(Failure::refused(format!(
            "git merge-base --is-ancestor {a} {b}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))),
    }
}

/// An abbreviated commit id for messages.
pub fn short(dir: &Path, oid: &str) -> String {
    git::run(dir, ["rev-parse", "--short", oid])
        .unwrap_or_else(|_| oid.get(..12).unwrap_or(oid).to_owned())
}
