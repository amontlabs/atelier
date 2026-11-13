//! Frozen-layer records (DESIGN §2.2, §2.4): branch records and the temp-folder registry. Their
//! format never changes, so any atelier version may read and write them. Behaviour (`start`,
//! `follow`, `tmp`, `clean`) comes in modules 3.3 and 3.5; this module fixes the files.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::exit::{Failure, IoContext, Result};
use crate::fsutil;
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pending {
    pub target: String,
    pub old_base: String,
}

/// One fact per branch: what it is built on (DESIGN §2.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchRecord {
    pub name: String,
    /// New for every `start`.
    pub lifetime: String,
    /// The branch it is built on: the integration branch or another item.
    pub lower: String,
    /// The lower branch's lifetime id; absent for the integration branch.
    pub lower_lifetime: Option<String>,
    /// The exact commit of what it is built on.
    pub base: String,
    pub state: BranchState,
    pub pending: Option<Pending>,
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
        Ok(Some(record))
    }

    pub fn write(&self, state: &State) -> Result<()> {
        let path = state.branch_record_path(&self.name);
        let text = toml::to_string(self).map_err(|e| Failure::refused(e.to_string()))?;
        fsutil::write_atomic(&path, text.as_bytes()).ctx(path.display())
    }
}

/// One registered temp folder: its exact path and file-system identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TmpEntry {
    pub path: PathBuf,
    pub dev: u64,
    pub ino: u64,
}

impl TmpEntry {
    /// Records an existing folder's identity.
    pub fn of(path: &Path) -> Result<TmpEntry> {
        let meta = fs::symlink_metadata(path).ctx(path.display())?;
        if !meta.is_dir() {
            return Err(Failure::refused(format!(
                "{} is not a real folder",
                path.display()
            )));
        }
        Ok(TmpEntry {
            path: path.to_path_buf(),
            dev: meta.dev(),
            ino: meta.ino(),
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
            Ok(()) => {}
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
            lower_lifetime: None,
            base: "a".repeat(40),
            state: BranchState::Active,
            pending: Some(Pending {
                target: "b".repeat(40),
                old_base: "a".repeat(40),
            }),
        };
        r.write(&state).unwrap();
        assert!(state.root().join("branches/disk%2Fui.toml").is_file());
        assert_eq!(BranchRecord::read(&state, "disk/ui").unwrap(), Some(r));
        assert_eq!(BranchRecord::read(&state, "disk%2Fui").unwrap(), None);
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
