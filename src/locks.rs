//! Locks (r4 §3.1): an exclusive `flock` on a file in the frozen layer, taken by the atelier
//! process and held until it exits. Never handed to the programs atelier starts (they get no
//! copy of the descriptor: Rust opens files close-on-exec), so a held lock means one thing only:
//! "an atelier process for this is alive".
//!
//! Next to each lock file, `<name>.procs` records what an atelier holding the lock started: the
//! run ids (`run <id>`) and each step's process group while it may have members
//! (`group <pgid> <id>`). Whoever takes the lock next reads it first and refuses (4) while a
//! recorded group still has a member or a process still carries a recorded run's marker. It
//! never stops them itself: a group id could have been reused by something unrelated.

use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::exit::{Failure, IoContext, Result};
use crate::fsutil;
use crate::git::Checkout;
use crate::names;
use crate::procs;
use crate::state::State;

/// What a lock protects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockKey {
    /// One worktree's runs (`check`, `test`).
    Worktree(String),
    /// One branch (`tmp`, `check`, `clean`).
    Branch(String),
    /// The gate.
    Gate,
}

impl LockKey {
    /// The run lock of the worktree a checkout belongs to, keyed by its git folder: the main
    /// checkout is `@main` (a key [`names::key`] never produces), a linked worktree is
    /// `worktrees%2F<name>`.
    pub fn worktree(checkout: &Checkout) -> LockKey {
        if checkout.is_main_worktree() {
            return LockKey::Worktree("@main".to_owned());
        }
        let git_dir = crate::git::canonical(&checkout.git_dir);
        let common = crate::git::canonical(&checkout.common_dir);
        let rel = git_dir
            .strip_prefix(&common)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| git_dir.to_string_lossy().into_owned());
        LockKey::Worktree(names::key(&rel))
    }

    /// The branch lock; the name must already be validated.
    pub fn branch(name: &str) -> LockKey {
        LockKey::Branch(name.to_owned())
    }

    fn rel(&self) -> PathBuf {
        match self {
            LockKey::Worktree(k) => Path::new("worktree").join(k),
            LockKey::Branch(name) => Path::new("branch").join(names::key(name)),
            LockKey::Gate => PathBuf::from("gate"),
        }
    }

    /// For messages and run results.
    pub fn describe(&self) -> String {
        match self {
            LockKey::Worktree(k) => format!("worktree lock {k}"),
            LockKey::Branch(name) => format!("branch lock {name}"),
            LockKey::Gate => "gate lock".to_owned(),
        }
    }
}

/// The processes recorded under a lock.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Record {
    pub runs: Vec<String>,
    pub groups: Vec<(i32, String)>,
}

impl Record {
    fn parse(text: &str) -> Record {
        let mut r = Record::default();
        for line in text.lines() {
            let parts: Vec<&str> = line.split(' ').collect();
            match parts.as_slice() {
                ["run", id] => r.runs.push((*id).to_owned()),
                ["group", pgid, id] => {
                    if let Ok(pgid) = pgid.parse() {
                        r.groups.push((pgid, (*id).to_owned()));
                    }
                }
                _ => {}
            }
        }
        r
    }

    fn render(&self) -> String {
        let mut s = String::new();
        for id in &self.runs {
            s.push_str(&format!("run {id}\n"));
        }
        for (pgid, id) in &self.groups {
            s.push_str(&format!("group {pgid} {id}\n"));
        }
        s
    }

    fn is_empty(&self) -> bool {
        self.runs.is_empty() && self.groups.is_empty()
    }
}

/// A lock held by this process, released when dropped (or when the process ends).
#[derive(Debug)]
pub struct HeldLock {
    key: LockKey,
    _file: File,
    record_path: PathBuf,
    record: Record,
}

impl HeldLock {
    /// Takes the lock without waiting. Refuses (4) when another atelier holds it, and when
    /// processes of an earlier run under it are still alive (listed, never signalled).
    pub fn acquire(state: &State, key: LockKey) -> Result<HeldLock> {
        let rel = key.rel();
        let lock_path = state.locks_dir().join(&rel).with_extension("lock");
        let record_path = state.locks_dir().join(&rel).with_extension("procs");
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).ctx(parent.display())?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .ctx(lock_path.display())?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(Failure::refused(format!(
                    "{} is held by another atelier command",
                    key.describe()
                )));
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(Failure::refused(format!("{}: {e}", lock_path.display())));
            }
        }
        let record = match fs::read_to_string(&record_path) {
            Ok(t) => Record::parse(&t),
            Err(e) if e.kind() == ErrorKind::NotFound => Record::default(),
            Err(e) => return Err(Failure::refused(format!("{}: {e}", record_path.display()))),
        };
        let mut held = HeldLock {
            key,
            _file: file,
            record_path,
            record,
        };
        held.check_earlier_runs()?;
        Ok(held)
    }

    pub fn key(&self) -> &LockKey {
        &self.key
    }

    pub fn record(&self) -> &Record {
        &self.record
    }

    fn check_earlier_runs(&mut self) -> Result<()> {
        if self.record.is_empty() {
            return Ok(());
        }
        let mut alive = Vec::new();
        for (pgid, id) in &self.record.groups {
            if procs::group_alive(*pgid) {
                let members = procs::group_members(*pgid);
                if members.is_empty() {
                    alive.push(format!("  process group {pgid} of run {id}"));
                }
                for m in members {
                    alive.push(format!("  {} (group {pgid}, run {id})", m.describe()));
                }
            }
        }
        let survivors = procs::marker_survivors(&self.record.runs)
            .map_err(|e| Failure::refused(format!("cannot list processes: {e}")))?;
        for (p, id) in survivors {
            if !alive
                .iter()
                .any(|l| l.starts_with(&format!("  {} ", p.pid)))
            {
                alive.push(format!(
                    "  {} (run {id}, {}={id})",
                    p.describe(),
                    procs::RUN_MARKER
                ));
            }
        }
        if !alive.is_empty() {
            return Err(Failure::refused(format!(
                "processes of an earlier run under the {} are still alive; atelier does not stop them, stop them and retry:\n{}",
                self.key.describe(),
                alive.join("\n")
            )));
        }
        self.record = Record::default();
        self.save()
    }

    /// Records a run that holds this lock.
    pub fn add_run(&mut self, id: &str) -> Result<()> {
        self.record.runs.push(id.to_owned());
        self.save()
    }

    /// Forgets a run: none of its processes is left.
    pub fn remove_run(&mut self, id: &str) -> Result<()> {
        self.record.runs.retain(|r| r != id);
        self.save()
    }

    /// Records a step's process group.
    pub fn add_group(&mut self, pgid: i32, run: &str) -> Result<()> {
        self.record.groups.push((pgid, run.to_owned()));
        self.save()
    }

    /// Forgets a process group: it is empty.
    pub fn remove_group(&mut self, pgid: i32) -> Result<()> {
        self.record.groups.retain(|(g, _)| *g != pgid);
        self.save()
    }

    fn save(&self) -> Result<()> {
        if self.record.is_empty() {
            return match fs::remove_file(&self.record_path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
                Err(e) => Err(Failure::refused(format!(
                    "{}: {e}",
                    self.record_path.display()
                ))),
            };
        }
        fsutil::write_atomic(&self.record_path, self.record.render().as_bytes())
            .ctx(self.record_path.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trips() {
        let r = Record {
            runs: vec!["20261113T1012Z-4f2a".into()],
            groups: vec![(1234, "20261113T1012Z-4f2a".into())],
        };
        assert_eq!(Record::parse(&r.render()), r);
    }

    #[test]
    fn a_held_lock_refuses_a_second_taker() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open_at(dir.path().join("atelier")).unwrap();
        let held = HeldLock::acquire(&state, LockKey::branch("a/b")).unwrap();
        let e = HeldLock::acquire(&state, LockKey::branch("a/b")).unwrap_err();
        assert_eq!(e.exit, crate::exit::Exit::Refused);
        assert!(e.message.contains("held by another atelier command"));
        // Another name does not alias.
        HeldLock::acquire(&state, LockKey::branch("a%2Fb")).unwrap();
        assert!(state.locks_dir().join("branch/a%2Fb.lock").is_file());
        assert!(state.locks_dir().join("branch/a%252%46b.lock").is_file());
        drop(held);
        HeldLock::acquire(&state, LockKey::branch("a/b")).unwrap();
    }

    #[test]
    fn a_dead_record_is_cleared_on_acquire() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open_at(dir.path().join("atelier")).unwrap();
        let mut held = HeldLock::acquire(&state, LockKey::Gate).unwrap();
        held.add_run("20000101T0000Z-dead").unwrap();
        // A process group id no live process has: pid_max is far below this on both systems.
        held.add_group(9_999_999, "20000101T0000Z-dead").unwrap();
        drop(held);
        let held = HeldLock::acquire(&state, LockKey::Gate).unwrap();
        assert_eq!(held.record(), &Record::default());
        assert!(!state.locks_dir().join("gate.procs").exists());
    }
}
