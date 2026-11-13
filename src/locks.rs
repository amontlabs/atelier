//! Locks (r4 §3.1): a `flock` on a file in the frozen layer, taken by the atelier process and
//! held until it exits. Never handed to the programs atelier starts (they get no copy of the
//! descriptor: Rust opens files close-on-exec), so a held lock means one thing only: "an atelier
//! process for this is alive".
//!
//! A branch lock is taken shared by the commands that only use the branch while they run (`tmp`,
//! and `check` in the branch's worktree: several may hold it at once) and exclusive by those that
//! change or delete it (`clean`, `follow`, `start`) (r4 §3.9 step 1). Every other lock is only
//! taken exclusive.
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
//!
//! An exclusive holder reads the record when it takes the lock, refuses while anything recorded
//! lives, and then owns the file. Shared holders run beside each other, so a shared holder never
//! refuses for what the record lists (it may be another holder's live run), and changes only its
//! own lines: each change re-reads the file and writes it back while holding
//! `locks/update/<same name>.lock` exclusive for that moment. Survivors of a shared holder therefore stop the next exclusive
//! taker (`clean`), not the next `tmp`.

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
///
/// The lock is free once its file is closed in every process. On Linux a child that another
/// thread is spawning holds a copy of the descriptor until it execs, so a lock released while
/// another thread spawns can look held for a moment. atelier spawns from one thread, and a spawn
/// returns only once the child has exec'd.
#[derive(Debug)]
pub struct HeldLock {
    key: LockKey,
    _file: File,
    record_path: PathBuf,
    /// Exclusive: the whole record. Shared: only this holder's lines.
    record: Record,
    shared: bool,
    /// `locks/update/<id>.lock`: held while a shared holder rewrites the record.
    update_lock: PathBuf,
}

/// `<locks>/<id><ext>`: the extension is appended, never put in place of a `.` the key has
/// (`branch/v1.2` and `branch/v1.3` are two locks).
fn lock_file(state: &State, key: &LockKey, ext: &str) -> PathBuf {
    let mut p = state.locks_dir().join(key.rel()).into_os_string();
    p.push(ext);
    PathBuf::from(p)
}

fn read_record(path: &std::path::Path, key: &LockKey) -> Result<Record> {
    match fs::read_to_string(path) {
        Ok(t) => Record::parse(&t).map_err(|m| {
            Failure::refused(format!(
                "{}: {m}; this atelier cannot tell what it records, so the {} stays refused until it is read by the version that wrote it or repaired by hand",
                path.display(),
                key.describe()
            ))
        }),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Record::default()),
        Err(e) => Err(Failure::refused(format!("{}: {e}", path.display()))),
    }
}

impl HeldLock {
    /// Takes the lock exclusive without waiting. Refuses (4) when another atelier holds it
    /// (shared or exclusive), and when processes of an earlier run under it are still alive
    /// (listed, never signalled).
    pub fn acquire(state: &State, key: LockKey) -> Result<HeldLock> {
        let mut held = Self::take(state, key, false)?;
        held.record = read_record(&held.record_path, &held.key)?;
        held.check_earlier_runs()?;
        Ok(held)
    }

    /// Takes a branch lock shared without waiting: other shared holders may hold it too.
    /// Refuses (4) while a command holds it exclusive. What the record lists is left to the next
    /// exclusive taker, but a record this version cannot read still refuses.
    pub fn acquire_shared(state: &State, key: LockKey) -> Result<HeldLock> {
        let held = Self::take(state, key, true)?;
        read_record(&held.record_path, &held.key)?;
        Ok(held)
    }

    fn take(state: &State, key: LockKey, shared: bool) -> Result<HeldLock> {
        let lock_path = lock_file(state, &key, ".lock");
        let record_path = lock_file(state, &key, ".procs");
        let update_lock = {
            let mut p = state
                .locks_dir()
                .join("update")
                .join(key.rel())
                .into_os_string();
            p.push(".lock");
            PathBuf::from(p)
        };
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
        let taken = if shared {
            file.try_lock_shared()
        } else {
            file.try_lock()
        };
        match taken {
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
        Ok(HeldLock {
            key,
            _file: file,
            record_path,
            record: Record::default(),
            shared,
            update_lock,
        })
    }

    pub fn is_shared(&self) -> bool {
        self.shared
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
        save(&self.record_path, &self.record)
    }

    /// Records a run that holds this lock.
    pub fn add_run(&mut self, id: &str) -> Result<()> {
        let line = RunLine {
            id: id.to_owned(),
            later: Vec::new(),
        };
        self.change(|r| r.runs.push(line.clone()))
    }

    /// Forgets a run: none of its processes is left.
    pub fn remove_run(&mut self, id: &str) -> Result<()> {
        self.change(|r| r.runs.retain(|l| l.id != id))
    }

    /// Records a step's process group with its leader's start time ([`procs::start_time`]).
    pub fn add_group(&mut self, pgid: i32, run: &str, leader_start: Option<String>) -> Result<()> {
        let line = GroupLine {
            pgid,
            run: run.to_owned(),
            leader_start,
            later: Vec::new(),
        };
        self.change(|r| r.groups.push(line.clone()))
    }

    /// Forgets a process group: it is empty.
    pub fn remove_group(&mut self, pgid: i32) -> Result<()> {
        self.change(|r| r.groups.retain(|g| g.pgid != pgid))
    }

    /// Applies one change to this holder's lines and writes the record. A shared holder applies
    /// it to the file as it is now, under its update lock, so the other holders' lines stay.
    fn change(&mut self, f: impl Fn(&mut Record)) -> Result<()> {
        f(&mut self.record);
        if !self.shared {
            return save(&self.record_path, &self.record);
        }
        // In a folder of its own: no lock key's file can be this one (a branch `a.procs` has
        // the lock file `branch/a.procs.lock`).
        let mutex_path = self.update_lock.clone();
        if let Some(parent) = mutex_path.parent() {
            fs::create_dir_all(parent).ctx(parent.display())?;
        }
        let mutex = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&mutex_path)
            .ctx(mutex_path.display())?;
        // Held for one read and one write; every holder keeps it that short, so waiting is safe.
        mutex.lock().ctx(mutex_path.display())?;
        let mut all = read_record(&self.record_path, &self.key)?;
        f(&mut all);
        save(&self.record_path, &all)
    }
}

/// Whether another process holds the lock right now, without taking it for good: the lock file
/// is opened only if it exists, and a shared lock taken to find out is released at once. Where
/// that cannot be told, `false`.
pub fn is_held(state: &State, key: &LockKey) -> bool {
    let path = lock_file(state, key, ".lock");
    let Ok(file) = OpenOptions::new().read(true).open(&path) else {
        return false;
    };
    matches!(
        file.try_lock_shared(),
        Err(std::fs::TryLockError::WouldBlock)
    )
}

/// The process groups the lock's record lists; none when it cannot be read.
pub fn recorded_groups(state: &State, key: &LockKey) -> Vec<GroupLine> {
    read_record(&lock_file(state, key, ".procs"), key)
        .map(|r| r.groups)
        .unwrap_or_default()
}

fn save(path: &std::path::Path, record: &Record) -> Result<()> {
    if record.is_empty() {
        return match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Failure::refused(format!("{}: {e}", path.display()))),
        };
    }
    fsutil::write_atomic(path, record.render().as_bytes()).ctx(path.display())
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

    /// `HeldLock::acquire` after this test released the lock: retried for a moment while it looks
    /// held, because a child another test thread is spawning holds a copy of the descriptor
    /// until it execs (Linux).
    fn acquire_after_release(state: &State, key: LockKey) -> HeldLock {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            match HeldLock::acquire(state, key.clone()) {
                Err(e)
                    if e.message.contains("held by another atelier command")
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                other => return other.unwrap(),
            }
        }
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
        acquire_after_release(&state, LockKey::Gate);
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
        let held = acquire_after_release(&state, LockKey::Gate);
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
        acquire_after_release(&state, LockKey::Gate);
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
        acquire_after_release(&state, LockKey::branch("a/b"));
    }

    #[test]
    fn shared_holders_run_together_and_an_exclusive_taker_waits_for_none_of_them() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open_at(dir.path().join("atelier")).unwrap();
        let key = LockKey::branch("a");
        let mut one = HeldLock::acquire_shared(&state, key.clone()).unwrap();
        let mut two = HeldLock::acquire_shared(&state, key.clone()).unwrap();
        assert!(one.is_shared());
        // clean, follow and start refuse while a shared holder runs.
        let e = HeldLock::acquire(&state, key.clone()).unwrap_err();
        assert_eq!(e.exit, crate::exit::Exit::Refused);
        assert!(e.message.contains("held by another atelier command"));
        // Each holder keeps the other's lines.
        let other = "20261113T1012Z-8899aabbccddeeff";
        one.add_run(ID).unwrap();
        two.add_run(other).unwrap();
        one.add_group(9_999_998, ID, None).unwrap();
        let path = state.locks_dir().join("branch/a.procs");
        let r = Record::parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(r.run_ids(), vec![ID.to_owned(), other.to_owned()]);
        assert_eq!(r.groups.len(), 1);
        one.remove_group(9_999_998).unwrap();
        one.remove_run(ID).unwrap();
        let r = Record::parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(r.run_ids(), vec![other.to_owned()]);
        two.remove_run(other).unwrap();
        assert!(!path.exists());
        drop((one, two));
        // An exclusive holder refuses a shared taker.
        let held = acquire_after_release(&state, key.clone());
        let e = HeldLock::acquire_shared(&state, key.clone()).unwrap_err();
        assert!(e.message.contains("held by another atelier command"));
        drop(held);
    }

    #[test]
    fn survivors_of_a_shared_holder_stop_the_next_exclusive_taker() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open_at(dir.path().join("atelier")).unwrap();
        let key = LockKey::branch("a");
        let child = sleeper_group();
        let pgid = child.id() as i32;
        let mut held = HeldLock::acquire_shared(&state, key.clone()).unwrap();
        held.add_group(pgid, ID, procs::start_time(pgid)).unwrap();
        drop(held);
        // Another shared taker runs beside it.
        HeldLock::acquire_shared(&state, key.clone()).unwrap();
        let e = loop {
            match HeldLock::acquire(&state, key.clone()) {
                Err(e) if e.message.contains("held by another") => {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                other => break other.unwrap_err(),
            }
        };
        assert!(e.message.contains("still alive"), "{}", e.message);
        stop(child);
        acquire_after_release(&state, key);
    }

    #[test]
    fn a_dot_in_a_key_is_not_an_extension() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open_at(dir.path().join("atelier")).unwrap();
        let _one = HeldLock::acquire(&state, LockKey::branch("v1.2")).unwrap();
        // Another branch whose name differs after the dot is another lock.
        HeldLock::acquire(&state, LockKey::branch("v1.3")).unwrap();
        assert!(state.locks_dir().join("branch/v1.2.lock").is_file());
        assert!(state.locks_dir().join("branch/v1.3.lock").is_file());
        // A shared holder's update lock is no other key's lock file (a branch `a.procs` has
        // `branch/a.procs.lock`).
        let mut held = HeldLock::acquire_shared(&state, LockKey::branch("a")).unwrap();
        held.add_run(ID).unwrap();
        assert!(state.locks_dir().join("update/branch/a.lock").is_file());
        assert!(!state.locks_dir().join("branch/a.procs.lock").exists());
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
        let held = acquire_after_release(&state, LockKey::Gate);
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
