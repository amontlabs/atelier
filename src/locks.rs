//! Locks (r4 §3.1): an exclusive `flock` on a file in the frozen layer, taken by the atelier
//! process and held until it exits. Never handed to the programs atelier starts (they get no
//! copy of the descriptor: Rust opens files close-on-exec), so a held lock means one thing only:
//! "an atelier process for this is alive".
//!
//! Next to each lock file, `<name>.procs` records what an atelier holding the lock started: the
//! run ids and each step's process group while it may have members. Whoever takes the lock next
//! reads it first and refuses (4) while a recorded group is still the one atelier started and
//! has a member, or a process still carries a recorded run's marker. It never stops them itself.
//!
//! The record's lines (frozen, `docs/FROZEN-LAYER.md`), fields separated by one space:
//!
//! ```text
//! run <run id> [<later fields>...]
//! group <pgid> <run id> <leader start time, or -> [<later fields>...]
//! ```
//!
//! Later versions may append fields; this version keeps them when it rewrites the file. Any other
//! non-empty line makes the record unreadable: taking the lock refuses (4) and names the file,
//! and nothing is rewritten.

use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::path::PathBuf;

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
    ///
    /// A key longer than [`names::MAX_KEY_LEN`] is cut and ends in `~` and a hash of the whole
    /// folder name (`~` never appears in an uncut key).
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
        LockKey::Worktree(worktree_key(&rel))
    }

    /// The branch lock; the name must already be validated.
    pub fn branch(name: &str) -> LockKey {
        LockKey::Branch(name.to_owned())
    }

    fn rel(&self) -> PathBuf {
        PathBuf::from(self.id())
    }

    /// The machine key a run records (`run.toml`'s `lock`): the lock file's path under
    /// `locks/` without its extension, `worktree/<key>`, `branch/<key>` or `gate`.
    pub fn id(&self) -> String {
        match self {
            LockKey::Worktree(k) => format!("worktree/{k}"),
            LockKey::Branch(name) => format!("branch/{}", names::key(name)),
            LockKey::Gate => "gate".to_owned(),
        }
    }

    /// The lock a machine key names; `None` when it is not one [`LockKey::id`] makes.
    pub fn from_id(id: &str) -> Option<LockKey> {
        if id == "gate" {
            return Some(LockKey::Gate);
        }
        if let Some(k) = id.strip_prefix("worktree/") {
            let ok = k == "@main"
                || (!k.is_empty()
                    && !k.contains('/')
                    && k.len() <= names::MAX_KEY_LEN + 17
                    && (names::unkey(k).is_some() || k.contains('~')));
            return ok.then(|| LockKey::Worktree(k.to_owned()));
        }
        let k = id.strip_prefix("branch/")?;
        names::unkey(k).map(LockKey::Branch)
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

fn worktree_key(rel: &str) -> String {
    let k = names::key(rel);
    if k.len() <= names::MAX_KEY_LEN {
        return k;
    }
    // FNV-1a, 64 bits: stable across Rust and atelier versions, unlike std's hasher.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in rel.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Keys are ASCII, so any byte offset is a character boundary.
    format!("{}~{h:016x}", &k[..names::MAX_KEY_LEN - 17])
}

/// A `run` line: a run that holds, or held, the lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunLine {
    pub id: String,
    /// Fields after the known ones, written by a later version; kept as they are.
    pub later: Vec<String>,
}

/// A `group` line: a step's process group that may still have members.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupLine {
    pub pgid: i32,
    pub run: String,
    /// The leader's [`procs::start_time`] when the group was recorded; `None` when unknown.
    pub leader_start: Option<String>,
    /// Fields after the known ones, written by a later version; kept as they are.
    pub later: Vec<String>,
}

/// The processes recorded under a lock.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Record {
    pub runs: Vec<RunLine>,
    pub groups: Vec<GroupLine>,
}

impl Record {
    /// Parses a record. `Err` names the first line this version cannot read.
    pub fn parse(text: &str) -> std::result::Result<Record, String> {
        let mut r = Record::default();
        for (n, line) in text.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.split(' ').collect();
            let bad = || format!("line {} is not a lock record line: {line:?}", n + 1);
            if parts
                .iter()
                .any(|p| p.is_empty() || p.contains(char::is_whitespace))
            {
                return Err(bad());
            }
            match parts.as_slice() {
                ["run", id, later @ ..] => r.runs.push(RunLine {
                    id: (*id).to_owned(),
                    later: later.iter().map(|s| (*s).to_owned()).collect(),
                }),
                ["group", pgid, run, start, later @ ..] => {
                    let pgid: i32 = pgid.parse().ok().filter(|p| *p > 1).ok_or_else(bad)?;
                    r.groups.push(GroupLine {
                        pgid,
                        run: (*run).to_owned(),
                        leader_start: (*start != "-").then(|| (*start).to_owned()),
                        later: later.iter().map(|s| (*s).to_owned()).collect(),
                    });
                }
                _ => return Err(bad()),
            }
        }
        Ok(r)
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        for r in &self.runs {
            s.push_str("run ");
            s.push_str(&r.id);
            for f in &r.later {
                s.push(' ');
                s.push_str(f);
            }
            s.push('\n');
        }
        for g in &self.groups {
            s.push_str(&format!(
                "group {} {} {}",
                g.pgid,
                g.run,
                g.leader_start.as_deref().unwrap_or("-")
            ));
            for f in &g.later {
                s.push(' ');
                s.push_str(f);
            }
            s.push('\n');
        }
        s
    }

    /// Every run id recorded, from `run` and `group` lines.
    pub fn run_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.runs.iter().map(|r| r.id.clone()).collect();
        for g in &self.groups {
            if !ids.contains(&g.run) {
                ids.push(g.run.clone());
            }
        }
        ids
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
            Ok(t) => Record::parse(&t).map_err(|m| {
                Failure::refused(format!(
                    "{}: {m}; this atelier cannot tell what it records, so the {} stays refused until it is read by the version that wrote it or repaired by hand",
                    record_path.display(),
                    key.describe()
                ))
            })?,
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
        for g in &self.record.groups {
            let (pgid, id) = (g.pgid, &g.run);
            if procs::recorded_group_alive(pgid, g.leader_start.as_deref()) {
                let members = procs::group_members(pgid);
                if members.is_empty() {
                    alive.push(format!("  process group {pgid} of run {id}"));
                }
                for m in members {
                    alive.push(format!("  {} (group {pgid}, run {id})", m.describe()));
                }
            }
        }
        let survivors = procs::marker_survivors(&self.record.run_ids())
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
                "processes of an earlier run are still alive ({}):\n{}",
                self.key.describe(),
                alive.join("\n")
            )));
        }
        self.record = Record::default();
        self.save()
    }

    /// Records a run that holds this lock.
    pub fn add_run(&mut self, id: &str) -> Result<()> {
        self.record.runs.push(RunLine {
            id: id.to_owned(),
            later: Vec::new(),
        });
        self.save()
    }

    /// Forgets a run: none of its processes is left.
    pub fn remove_run(&mut self, id: &str) -> Result<()> {
        self.record.runs.retain(|r| r.id != id);
        self.save()
    }

    /// Records a step's process group with its leader's start time ([`procs::start_time`]).
    pub fn add_group(&mut self, pgid: i32, run: &str, leader_start: Option<String>) -> Result<()> {
        self.record.groups.push(GroupLine {
            pgid,
            run: run.to_owned(),
            leader_start,
            later: Vec::new(),
        });
        self.save()
    }

    /// Forgets a process group: it is empty.
    pub fn remove_group(&mut self, pgid: i32) -> Result<()> {
        self.record.groups.retain(|g| g.pgid != pgid);
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
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};

    const ID: &str = "20261113T1012Z-0011223344556677";

    #[test]
    fn record_round_trips_and_keeps_later_fields() {
        let text = format!(
            "run {ID}\nrun {ID} later-field x\ngroup 1234 {ID} 1794573762.373521\ngroup 99 {ID} - more\n"
        );
        let r = Record::parse(&text).unwrap();
        assert_eq!(r.runs[1].later, vec!["later-field", "x"]);
        assert_eq!(
            r.groups[0].leader_start.as_deref(),
            Some("1794573762.373521")
        );
        assert_eq!(r.groups[1].leader_start, None);
        assert_eq!(r.groups[1].later, vec!["more"]);
        assert_eq!(r.render(), text);
        assert_eq!(Record::parse(&r.render()).unwrap(), r);
    }

    #[test]
    fn unreadable_record_lines_are_refused() {
        for bad in [
            format!("group 12x {ID} -"),
            format!("group  1234 {ID} -"),
            format!("group 1234 {ID}"),
            format!("group 1 {ID} -"),
            format!("group -5 {ID} -"),
            format!("run {ID} "),
            "run".to_owned(),
            format!("process 1234 {ID}"),
            format!("group\t1234 {ID} -"),
        ] {
            assert!(Record::parse(&bad).is_err(), "{bad:?}");
        }
        assert_eq!(Record::parse("\n\n").unwrap(), Record::default());
    }

    fn sleeper_group() -> Child {
        Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap()
    }

    fn stop(mut child: Child) {
        let pgid = child.id() as i32;
        procs::stop_group(pgid, Some(&mut child), std::time::Duration::from_secs(2));
    }

    #[test]
    fn a_live_group_on_a_line_with_later_fields_still_refuses_and_the_line_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open_at(dir.path().join("atelier")).unwrap();
        let child = sleeper_group();
        let pgid = child.id() as i32;
        let start = procs::start_time(pgid).unwrap();
        let path = state.locks_dir().join("gate.procs");
        let text = format!("group {pgid} {ID} {start} 123456\n");
        fs::write(&path, &text).unwrap();
        let e = HeldLock::acquire(&state, LockKey::Gate).unwrap_err();
        assert_eq!(e.exit, crate::exit::Exit::Refused);
        assert!(
            e.message
                .contains("processes of an earlier run are still alive"),
            "{}",
            e.message
        );
        assert!(!e.message.contains("stop them"), "{}", e.message);
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        stop(child);
        HeldLock::acquire(&state, LockKey::Gate).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn an_unreadable_record_refuses_and_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open_at(dir.path().join("atelier")).unwrap();
        let path = state.locks_dir().join("gate.procs");
        let text = format!("run {ID}\ngroup  4242 {ID} -\nwatch 4242 {ID}\n");
        fs::write(&path, &text).unwrap();
        let e = HeldLock::acquire(&state, LockKey::Gate).unwrap_err();
        assert_eq!(e.exit, crate::exit::Exit::Refused);
        assert!(e.message.contains("gate.procs: line 2"), "{}", e.message);
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn a_reused_pgid_does_not_refuse() {
        // The recorded pgid now leads a group that started later: not the recorded group.
        let dir = tempfile::tempdir().unwrap();
        let state = State::open_at(dir.path().join("atelier")).unwrap();
        let child = sleeper_group();
        let pgid = child.id() as i32;
        fs::write(
            state.locks_dir().join("gate.procs"),
            format!("group {pgid} {ID} 1000.000000\n"),
        )
        .unwrap();
        let held = HeldLock::acquire(&state, LockKey::Gate).unwrap();
        assert_eq!(held.record(), &Record::default());
        drop(held);
        // The review's probe: a group led by a process this user did not start (a daemon),
        // recorded with another start time.
        let me = std::process::id() as i32;
        // SAFETY: getpgrp has no preconditions.
        let own = unsafe { libc::getpgrp() };
        let other = procs::list()
            .unwrap()
            .into_iter()
            .find(|p| p.pid == p.pgid && p.pid > 1 && p.pid != me && p.pgid != own && p.pid != pgid)
            .expect("some other group leader");
        fs::write(
            state.locks_dir().join("gate.procs"),
            format!("group {} {ID} 1000.000000\n", other.pid),
        )
        .unwrap();
        HeldLock::acquire(&state, LockKey::Gate).unwrap();
        stop(child);
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
        held.add_run(ID).unwrap();
        // A process group id no live process has: pid_max is far below this on both systems.
        held.add_group(9_999_999, ID, None).unwrap();
        drop(held);
        let held = HeldLock::acquire(&state, LockKey::Gate).unwrap();
        assert_eq!(held.record(), &Record::default());
        assert!(!state.locks_dir().join("gate.procs").exists());
    }

    #[test]
    fn lock_ids_are_machine_keys() {
        for key in [
            LockKey::Gate,
            LockKey::branch("a/b"),
            LockKey::branch("Foo"),
            LockKey::Worktree("@main".into()),
            LockKey::Worktree("worktrees%2Fwt".into()),
        ] {
            assert_eq!(LockKey::from_id(&key.id()), Some(key.clone()), "{key:?}");
        }
        assert_eq!(LockKey::branch("a/b").id(), "branch/a%2Fb");
        assert_eq!(LockKey::from_id("branch lock a/b"), None);
        assert_eq!(LockKey::from_id("branch/a/b"), None);
        assert_eq!(LockKey::from_id("worktree/../x"), None);
    }

    #[test]
    fn long_worktree_keys_are_capped_and_stay_distinct() {
        let a = format!("worktrees/{}", "a".repeat(300));
        let b = format!("worktrees/{}b", "a".repeat(299));
        let (ka, kb) = (worktree_key(&a), worktree_key(&b));
        assert!(ka.len() <= names::MAX_KEY_LEN, "{}", ka.len());
        assert_ne!(ka, kb);
        assert_eq!(ka, worktree_key(&a));
        assert_eq!(worktree_key("worktrees/wt"), "worktrees%2Fwt");
        assert_eq!(
            LockKey::from_id(&format!("worktree/{ka}")),
            Some(LockKey::Worktree(ka.clone()))
        );
        // The lock file can be created.
        let dir = tempfile::tempdir().unwrap();
        let state = State::open_at(dir.path().join("atelier")).unwrap();
        HeldLock::acquire(&state, LockKey::Worktree(ka)).unwrap();
    }
}
