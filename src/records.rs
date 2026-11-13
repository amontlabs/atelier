//! Frozen-layer records: branch records and the temp-folder registry. Their
//! format is frozen (`docs/FROZEN-LAYER.md`), so any atelier version may read and write them:
//! later versions may add fields, never change or remove one. A reader keeps the fields it does
//! not know (`later`) and writes them back unchanged. Behaviour (`start`, `follow`, `tmp`,
//! `clean`) comes in other modules; this module fixes the files.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::exit::{Failure, IoContext, Result};
use crate::fsutil;
use crate::refs::check_oid;
use crate::state::State;

/// A branch's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BranchState {
    Active,
    Merged,
    Cleaned,
}

/// A `follow` that started a rebase and has not been settled yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub target: String,
    pub old_base: String,
    /// Fields written by a later version, kept as they are.
    #[serde(flatten)]
    pub later: toml::Table,
}

/// A `start` that has not finished. Written with the record before the branch ref exists and
/// removed once the branch is checked out, so a later `start` of the same name can finish or
/// undo it (`docs/FROZEN-LAYER.md`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Starting {
    /// The worktree the branch is checked out in: an absolute path, UTF-8 only.
    pub worktree: String,
    /// True when `start` creates that worktree (`--path`); false when it switches the detached
    /// worktree it runs in.
    pub new_worktree: bool,
    /// Fields written by a later version, kept as they are.
    #[serde(flatten)]
    pub later: toml::Table,
}

/// One fact per branch: what it is built on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BranchRecord {
    pub name: String,
    /// New for every `start`.
    pub lifetime: String,
    /// The branch it is built on: the integration branch or another item.
    pub lower: String,
    /// The lower branch is the integration branch (then `lower_lifetime` is absent).
    pub lower_is_integration: bool,
    /// The lower branch's lifetime id; absent exactly when the lower branch is the integration
    /// branch.
    pub lower_lifetime: Option<String>,
    /// The exact commit of what it is built on.
    pub base: String,
    pub state: BranchState,
    pub pending: Option<Pending>,
    /// Set while the branch's `start` has not finished.
    pub starting: Option<Starting>,
    /// Fields written by a later version, kept as they are.
    #[serde(flatten)]
    pub later: toml::Table,
}

/// A fresh lifetime id.
pub fn new_lifetime_id() -> Result<String> {
    Ok(fsutil::random_hex(8)?)
}

impl BranchRecord {
    pub fn read(state: &State, branch: &str) -> Result<Option<BranchRecord>> {
        let path = state.branch_record_path(branch);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Failure::refused(format!("{}: {e}", path.display()))),
        };
        let record: BranchRecord = toml::from_str(&text).map_err(|e| {
            Failure::refused(format!("{}: {}", path.display(), e.to_string().trim()))
        })?;
        if record.name != branch {
            return Err(Failure::refused(format!(
                "{} names branch {:?}, not {branch:?}",
                path.display(),
                record.name
            )));
        }
        record.validate().map_err(|f| f.context(path.display()))?;
        Ok(Some(record))
    }

    /// Checks what later commands rely on: full commit ids (they reach `git rebase --onto`),
    /// atelier ids, and a lower lifetime exactly when the lower branch is not the integration
    /// branch.
    pub fn validate(&self) -> Result<()> {
        check_plain_id(&self.lifetime)?;
        check_oid(&self.base).map_err(|f| Failure::refused(f.message).context("base"))?;
        if let Some(p) = &self.pending {
            check_oid(&p.target)
                .map_err(|f| Failure::refused(f.message).context("pending.target"))?;
            check_oid(&p.old_base)
                .map_err(|f| Failure::refused(f.message).context("pending.old_base"))?;
        }
        match (&self.lower_lifetime, self.lower_is_integration) {
            (None, true) => Ok(()),
            (Some(l), false) => check_plain_id(l),
            (Some(_), true) => Err(Failure::refused(
                "lower_is_integration is true but lower_lifetime is set",
            )),
            (None, false) => Err(Failure::refused(
                "lower_lifetime is missing for a lower branch that is not the integration branch",
            )),
        }
    }

    pub fn write(&self, state: &State) -> Result<()> {
        self.validate()?;
        let path = state.branch_record_path(&self.name);
        let text = toml::to_string(self).map_err(|e| Failure::refused(e.to_string()))?;
        fsutil::write_atomic(&path, text.as_bytes()).ctx(path.display())
    }
}

/// What `clean` may trust about a merged branch: the commit `merge` merged and the worktree
/// `approve` recorded (path and file-system identity). It is the `merged` table of the branch's
/// record (`docs/FROZEN-LAYER.md`), written by `merge` in the same write that sets the state to
/// `merged` ([`MergeFacts::record`]) and removed by `clean` in the write that sets `cleaned`
/// ([`MergeFacts::retire`]). A merged record without it grants `clean` nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MergeFacts {
    /// The full commit id the branch was merged at: its tip when approved.
    pub commit: String,
    /// The one worktree `clean` may delete for this branch: absolute, canonical, UTF-8.
    pub worktree: PathBuf,
    pub dev: u64,
    pub ino: u64,
    /// Fields written by a later version, kept as they are.
    #[serde(flatten)]
    pub later: toml::Table,
}

impl MergeFacts {
    /// The record's key for these facts.
    const KEY: &'static str = "merged";

    /// Records the identity of `worktree` (an existing folder) as `approve` does.
    pub fn of_worktree(commit: &str, worktree: &Path) -> Result<MergeFacts> {
        let entry = TmpEntry::of(worktree)?;
        Ok(MergeFacts {
            commit: commit.to_owned(),
            worktree: entry.path,
            dev: entry.dev,
            ino: entry.ino,
            later: toml::Table::new(),
        })
    }

    /// The facts a record holds. `None` when it has none; refuses (4) a table this version
    /// cannot read.
    pub fn read(rec: &BranchRecord) -> Result<Option<MergeFacts>> {
        let Some(v) = rec.later.get(Self::KEY) else {
            return Ok(None);
        };
        let facts: MergeFacts = v.clone().try_into().map_err(|e: toml::de::Error| {
            Failure::refused(format!("{}: merged: {}", rec.name, e.to_string().trim()))
        })?;
        check_oid(&facts.commit)
            .map_err(|f| Failure::refused(f.message).context("merged.commit"))?;
        if !facts.worktree.is_absolute() || facts.worktree.to_str().is_none() {
            return Err(Failure::refused(format!(
                "{}: merged.worktree must be an absolute UTF-8 path",
                rec.name
            )));
        }
        Ok(Some(facts))
    }

    /// The one write `merge` makes for a branch: state `merged` with these facts, only for the
    /// given lifetime. The caller holds the branch's lock.
    pub fn record(&self, state: &State, branch: &str, lifetime: &str) -> Result<()> {
        let mut rec = of_lifetime(state, branch, lifetime)?;
        let table = toml::Table::try_from(self).map_err(|e| Failure::refused(e.to_string()))?;
        rec.later
            .insert(Self::KEY.to_owned(), toml::Value::Table(table));
        rec.state = BranchState::Merged;
        rec.write(state)
    }

    /// The one write `clean` makes when everything is gone: state `cleaned`, facts removed (they
    /// grant nothing more). The caller holds the branch's lock.
    pub fn retire(state: &State, branch: &str, lifetime: &str) -> Result<()> {
        let mut rec = of_lifetime(state, branch, lifetime)?;
        rec.later.remove(Self::KEY);
        rec.state = BranchState::Cleaned;
        rec.write(state)
    }
}

fn of_lifetime(state: &State, branch: &str, lifetime: &str) -> Result<BranchRecord> {
    BranchRecord::read(state, branch)?
        .filter(|r| r.lifetime == lifetime)
        .ok_or_else(|| {
            Failure::refused(format!(
                "{branch}: no record of lifetime {lifetime}; it was started again"
            ))
        })
}

/// One registered temp folder: its exact path and file-system identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TmpEntry {
    /// Canonical: no symbolic link anywhere in it (`/private/var/...`, not `/var/...`).
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
    /// Fields written by a later version, kept as they are.
    #[serde(flatten)]
    pub later: toml::Table,
}

impl TmpEntry {
    /// Records an existing folder's identity under its canonical path. Refuses (4) a path that
    /// is not a real folder (a link to one included) and one that is not UTF-8.
    pub fn of(path: &Path) -> Result<TmpEntry> {
        let meta = fs::symlink_metadata(path).ctx(path.display())?;
        if !meta.is_dir() {
            return Err(Failure::refused(format!(
                "{} is not a real folder",
                path.display()
            )));
        }
        // The last component is not a link (checked above), so this only resolves the parents.
        let canonical = fs::canonicalize(path).ctx(path.display())?;
        if canonical.to_str().is_none() {
            return Err(Failure::refused(format!(
                "{} is not a UTF-8 path; atelier registers only UTF-8 paths",
                path.display()
            )));
        }
        let meta = fs::symlink_metadata(&canonical).ctx(canonical.display())?;
        Ok(TmpEntry {
            path: canonical,
            dev: meta.dev(),
            ino: meta.ino(),
            later: toml::Table::new(),
        })
    }

    /// True when the path still names the same real folder.
    pub fn still_matches(&self) -> bool {
        fs::symlink_metadata(&self.path)
            .map(|m| m.is_dir() && m.dev() == self.dev && m.ino() == self.ino)
            .unwrap_or(false)
    }

    /// Registers the folder under a branch lifetime id; `id` names this folder's entry.
    pub fn register(&self, state: &State, lifetime: &str, id: &str) -> Result<PathBuf> {
        check_plain_id(lifetime)?;
        check_plain_id(id)?;
        let dir = state.tmp_dir().join(lifetime);
        match fs::create_dir(&dir) {
            Ok(()) => fs::File::open(state.tmp_dir())
                .and_then(|d| d.sync_all())
                .ctx(state.tmp_dir().display())?,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(Failure::refused(format!("{}: {e}", dir.display()))),
        }
        let file = dir.join(format!("{id}.toml"));
        let text = toml::to_string(self).map_err(|e| Failure::refused(e.to_string()))?;
        fsutil::write_atomic(&file, text.as_bytes()).ctx(file.display())?;
        Ok(file)
    }

    /// Every folder registered under a lifetime id.
    pub fn list(state: &State, lifetime: &str) -> Result<Vec<(PathBuf, TmpEntry)>> {
        check_plain_id(lifetime)?;
        let dir = state.tmp_dir().join(lifetime);
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Failure::refused(format!("{}: {e}", dir.display()))),
        };
        let mut out = Vec::new();
        for entry in entries {
            let path = entry.ctx(dir.display())?.path();
            if path.extension().is_none_or(|x| x != "toml") {
                continue;
            }
            let text = fs::read_to_string(&path).ctx(path.display())?;
            let e: TmpEntry = toml::from_str(&text).map_err(|e| {
                Failure::refused(format!("{}: {}", path.display(), e.to_string().trim()))
            })?;
            out.push((path, e));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}

/// Ids made by atelier: lowercase hex and `-`, used as one path segment.
fn check_plain_id(id: &str) -> Result<()> {
    if !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase() || b == b'-')
    {
        Ok(())
    } else {
        Err(Failure::refused(format!("not an atelier id: {id:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_records_round_trip_under_encoded_names() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open_at(dir.path().join("atelier")).unwrap();
        let r = BranchRecord {
            name: "disk/ui".into(),
            lifetime: new_lifetime_id().unwrap(),
            lower: "main".into(),
            lower_is_integration: true,
            lower_lifetime: None,
            base: "a".repeat(40),
            state: BranchState::Active,
            pending: Some(Pending {
                target: "b".repeat(40),
                old_base: "a".repeat(40),
                later: toml::Table::new(),
            }),
            starting: Some(Starting {
                worktree: "/w/disk-ui".into(),
                new_worktree: true,
                later: toml::Table::new(),
            }),
            later: toml::Table::new(),
        };
        r.write(&state).unwrap();
        assert!(state.root().join("branches/disk%2Fui.toml").is_file());
        assert_eq!(BranchRecord::read(&state, "disk/ui").unwrap(), Some(r));
        assert_eq!(BranchRecord::read(&state, "disk%2Fui").unwrap(), None);
    }

    #[test]
    fn later_fields_are_read_and_written_back() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open_at(dir.path().join("atelier")).unwrap();
        let path = state.branch_record_path("x");
        let a = "a".repeat(40);
        let b = "b".repeat(64);
        fs::write(
            &path,
            format!(
                "name = \"x\"\nlifetime = \"00ff\"\nlower = \"y\"\nlower_is_integration = false\nlower_lifetime = \"11ee\"\nbase = \"{a}\"\nstate = \"active\"\nreviewed_by = \"later\"\n\n[pending]\ntarget = \"{b}\"\nold_base = \"{a}\"\nstep = 3\n\n[starting]\nworktree = \"/w/x\"\nnew_worktree = true\nhint = 7\n\n[extra]\nkey = [1, 2]\n"
            ),
        )
        .unwrap();
        let r = BranchRecord::read(&state, "x").unwrap().unwrap();
        assert_eq!(r.later["reviewed_by"].as_str(), Some("later"));
        assert!(r.later["extra"].is_table());
        assert_eq!(
            r.pending.as_ref().unwrap().later["step"].as_integer(),
            Some(3)
        );
        assert_eq!(
            r.starting.as_ref().unwrap().later["hint"].as_integer(),
            Some(7)
        );
        r.write(&state).unwrap();
        assert_eq!(BranchRecord::read(&state, "x").unwrap().unwrap(), r);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("reviewed_by = \"later\""), "{text}");
        assert!(text.contains("step = 3"), "{text}");
        assert!(text.contains("hint = 7"), "{text}");
    }

    #[test]
    fn records_with_bad_commits_or_lower_fields_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open_at(dir.path().join("atelier")).unwrap();
        let a = "a".repeat(40);
        for (body, why) in [
            (
                "lower_is_integration = true\nbase = \"HEAD~1\"\n".to_owned(),
                "base",
            ),
            (
                "lower_is_integration = true\nbase = \"--upload-pack=x\"\n".to_owned(),
                "base",
            ),
            (
                format!(
                    "lower_is_integration = true\nbase = \"{a}\"\n[pending]\ntarget = \"main\"\nold_base = \"{a}\"\n"
                ),
                "pending.target",
            ),
            (
                format!("lower_is_integration = false\nbase = \"{a}\"\n"),
                "lower_lifetime",
            ),
            (
                format!("lower_is_integration = true\nlower_lifetime = \"00ff\"\nbase = \"{a}\"\n"),
                "lower_is_integration",
            ),
            (format!("base = \"{a}\"\n"), "lower_is_integration"),
        ] {
            let path = state.branch_record_path("x");
            let text = format!(
                "name = \"x\"\nlifetime = \"00ff\"\nlower = \"main\"\nstate = \"active\"\n{body}"
            );
            // `state` must come before any table: put tables last.
            fs::write(&path, text).unwrap();
            let e = BranchRecord::read(&state, "x").unwrap_err();
            assert_eq!(e.exit, crate::exit::Exit::Refused);
            assert!(e.message.contains(why), "{why}: {}", e.message);
        }
    }

    #[test]
    fn tmp_entries_store_the_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let real = fs::canonicalize(dir.path()).unwrap();
        let link = real.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        fs::create_dir(real.join("t")).unwrap();
        // Through a linked parent: the entry names the real path.
        let e = TmpEntry::of(&link.join("t")).unwrap();
        assert_eq!(e.path, real.join("t"));
        assert!(e.still_matches());
        // A link as the folder itself is refused.
        assert!(TmpEntry::of(&link).is_err());
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::OsStrExt;
            let odd = real.join(std::ffi::OsStr::from_bytes(b"x\xff"));
            fs::create_dir(&odd).unwrap();
            assert!(TmpEntry::of(&odd).unwrap_err().message.contains("UTF-8"));
        }
    }

    #[test]
    fn tmp_registry_records_identity() {
        let dir = tempfile::tempdir().unwrap();
        let state = State::open_at(dir.path().join("atelier")).unwrap();
        let folder = dir.path().join("t1");
        fs::create_dir(&folder).unwrap();
        let e = TmpEntry::of(&folder).unwrap();
        e.register(&state, "0011aabb", "f1").unwrap();
        let listed = TmpEntry::list(&state, "0011aabb").unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].1.still_matches());
        // A link put at the registered path is not the registered folder.
        fs::remove_dir(&folder).unwrap();
        std::os::unix::fs::symlink(dir.path(), &folder).unwrap();
        assert!(!listed[0].1.still_matches());
        assert!(e.register(&state, "../x", "f1").is_err());
        assert!(e.register(&state, "0011aabb", "a/b").is_err());
    }
}
