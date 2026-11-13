//! The versioned layer's records (DESIGN §2.2, §3 `approve`, `merge`, `promote`), format 1:
//! approvals, the merge record (`merges.tsv`) and the promotion record. Only the version pinned
//! on the integration branch reads or writes them, in the main checkout (`preflight`), so their
//! format may change between versions with a converter (`state::Converter`). The layout is in
//! `docs/MERGE.md`.
//!
//! - `versioned/approvals/<key>.toml`: one per branch, replaced by every `approve`, removed when
//!   the branch is merged.
//! - `versioned/merges.tsv`: one line per merged branch, appended once.
//! - `versioned/promotion.toml`: the promotion of a green gate that has not been recorded yet.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::exit::{Failure, IoContext, Result};
use crate::fsutil;
use crate::names;
use crate::records::MergeFacts;
use crate::refs::check_oid;
use crate::state::Versioned;

/// That a branch's exact commit was reviewed and may be merged, and the one worktree it was
/// built in (the only one `clean` may delete for it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub branch: String,
    /// The branch record's lifetime when it was approved.
    pub lifetime: String,
    /// The approved commit: the branch's tip at approval.
    pub commit: String,
    /// The worktree's canonical path.
    pub worktree: PathBuf,
    /// The worktree folder's file-system identity.
    pub dev: u64,
    pub ino: u64,
    /// RFC 3339 UTC.
    pub approved: String,
}

impl Approval {
    /// The merge facts this approval grants once its commit is merged.
    pub fn facts(&self) -> MergeFacts {
        MergeFacts {
            commit: self.commit.clone(),
            worktree: self.worktree.clone(),
            dev: self.dev,
            ino: self.ino,
            later: toml::Table::new(),
        }
    }
}

fn approvals_dir(v: &Versioned) -> PathBuf {
    v.dir().join("approvals")
}

fn approval_path(v: &Versioned, branch: &str) -> PathBuf {
    approvals_dir(v).join(format!("{}.toml", names::key(branch)))
}

impl Approval {
    pub fn read(v: &Versioned, branch: &str) -> Result<Option<Approval>> {
        let path = approval_path(v, branch);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Failure::refused(format!("{}: {e}", path.display()))),
        };
        let a: Approval = toml::from_str(&text).map_err(|e| {
            Failure::refused(format!("{}: {}", path.display(), e.to_string().trim()))
        })?;
        if a.branch != branch {
            return Err(Failure::refused(format!(
                "{} names branch {:?}, not {branch:?}",
                path.display(),
                a.branch
            )));
        }
        check_oid(&a.commit).map_err(|f| Failure::refused(f.message).context(path.display()))?;
        Ok(Some(a))
    }

    pub fn write(&self, v: &Versioned) -> Result<()> {
        check_oid(&self.commit)?;
        let dir = approvals_dir(v);
        fs::create_dir_all(&dir).ctx(dir.display())?;
        let path = approval_path(v, &self.branch);
        let text = toml::to_string(self).map_err(|e| Failure::refused(e.to_string()))?;
        fsutil::write_atomic(&path, text.as_bytes()).ctx(path.display())
    }

    /// Removes a branch's approval, if any.
    pub fn remove(v: &Versioned, branch: &str) -> Result<()> {
        let path = approval_path(v, branch);
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Failure::refused(format!("{}: {e}", path.display()))),
        }
    }
}

/// How a platform passed for a merge: where it ran, or the owner's waiver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformRecord {
    pub platform: String,
    /// `pass` or `waived`.
    pub result: String,
    /// The `machines.toml` name it ran on; absent for this computer.
    pub machine: Option<String>,
    /// The owner's words, for a waiver.
    pub reason: Option<String>,
    /// The run that tested it (the inner `check`'s run id, or the gate's own).
    pub run: Option<String>,
}

impl PlatformRecord {
    /// `macos-aarch64 PASS on this computer`, `linux-x86_64 WAIVED (<reason>)`.
    pub fn describe(&self) -> String {
        match self.result.as_str() {
            "waived" => format!(
                "{} WAIVED ({})",
                self.platform,
                self.reason.as_deref().unwrap_or("")
            ),
            _ => format!(
                "{} PASS on {}",
                self.platform,
                self.machine.as_deref().unwrap_or("this computer")
            ),
        }
    }

    /// The `merges.tsv` form: `<platform>=pass@<machine>` or `<platform>=waived:<reason>`.
    fn tsv(&self) -> String {
        match self.result.as_str() {
            "waived" => format!(
                "{}=waived:{}",
                self.platform,
                self.reason.as_deref().unwrap_or("")
            ),
            r => format!(
                "{}={r}@{}",
                self.platform,
                self.machine.as_deref().unwrap_or("local")
            ),
        }
    }
}

/// One branch of a promotion, with what `approve` recorded of its worktree (the merge facts
/// `clean` trusts, written into the branch record when the promotion is recorded).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotedBranch {
    pub name: String,
    pub lifetime: String,
    /// The approved commit that was merged.
    pub commit: String,
    pub worktree: PathBuf,
    pub dev: u64,
    pub ino: u64,
}

/// A green gate's promotion, written before the push and removed once every branch is recorded.
/// The next `merge` or `promote` reads it and asks git three questions (r4 §3.8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Promotion {
    pub integration: String,
    /// The integration branch's commit the gate merged onto.
    pub old: String,
    /// The merged, checked commit.
    pub m: String,
    /// The gate's run id.
    pub run: String,
    pub created: String,
    /// The last push: when it started, its process group, and when it ended. A push that
    /// started and did not end was interrupted, or atelier was.
    pub push_started: Option<String>,
    pub push_pgid: Option<i32>,
    pub push_ended: Option<String>,
    /// The gate's GREEN line, printed when the promotion is recorded.
    pub summary: Option<String>,
    /// A branch of the batch changed `.atelier/` or the generated rules.
    #[serde(default)]
    pub rules_changed: bool,
    pub branches: Vec<PromotedBranch>,
    pub platforms: Vec<PlatformRecord>,
}

fn promotion_path(v: &Versioned) -> PathBuf {
    v.dir().join("promotion.toml")
}

impl Promotion {
    pub fn read(v: &Versioned) -> Result<Option<Promotion>> {
        let path = promotion_path(v);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Failure::refused(format!("{}: {e}", path.display()))),
        };
        let p: Promotion = toml::from_str(&text).map_err(|e| {
            Failure::refused(format!("{}: {}", path.display(), e.to_string().trim()))
        })?;
        for oid in [&p.old, &p.m] {
            check_oid(oid).map_err(|f| Failure::refused(f.message).context(path.display()))?;
        }
        Ok(Some(p))
    }

    pub fn write(&self, v: &Versioned) -> Result<()> {
        let path = promotion_path(v);
        let text = toml::to_string(self).map_err(|e| Failure::refused(e.to_string()))?;
        fsutil::write_atomic(&path, text.as_bytes()).ctx(path.display())
    }

    pub fn remove(v: &Versioned) -> Result<()> {
        let path = promotion_path(v);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(Failure::refused(format!("{}: {e}", path.display()))),
        }
        if let Some(dir) = path.parent() {
            let _ = fs::File::open(dir).and_then(|d| d.sync_all());
        }
        Ok(())
    }

    /// A push started and did not end.
    pub fn push_unfinished(&self) -> bool {
        self.push_started.is_some() && self.push_ended.is_none()
    }
}

/// One line of `merges.tsv`: a merged branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeLine {
    pub time: String,
    pub branch: String,
    pub lifetime: String,
    /// The merged (approved) commit.
    pub commit: String,
    pub old: String,
    pub m: String,
    pub run: String,
    pub platforms: Vec<String>,
}

fn merges_path(v: &Versioned) -> PathBuf {
    v.dir().join("merges.tsv")
}

/// `%`, tab, newline, carriage return and `;` percent-encoded: fields never hold a separator.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' | '\t' | '\n' | '\r' | ';' => out.push_str(&format!("%{:02X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl MergeLine {
    pub fn new(p: &Promotion, b: &PromotedBranch) -> MergeLine {
        MergeLine {
            time: fsutil::rfc3339(fsutil::now_secs()),
            branch: b.name.clone(),
            lifetime: b.lifetime.clone(),
            commit: b.commit.clone(),
            old: p.old.clone(),
            m: p.m.clone(),
            run: p.run.clone(),
            platforms: p.platforms.iter().map(PlatformRecord::tsv).collect(),
        }
    }

    fn render(&self) -> String {
        let platforms: Vec<String> = self.platforms.iter().map(|p| escape(p)).collect();
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            escape(&self.time),
            escape(&self.branch),
            escape(&self.lifetime),
            self.commit,
            self.old,
            self.m,
            escape(&self.run),
            platforms.join(";")
        )
    }

    fn parse(line: &str) -> Option<MergeLine> {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 8 {
            return None;
        }
        Some(MergeLine {
            time: unescape(f[0]),
            branch: unescape(f[1]),
            lifetime: unescape(f[2]),
            commit: f[3].to_owned(),
            old: f[4].to_owned(),
            m: f[5].to_owned(),
            run: unescape(f[6]),
            platforms: if f[7].is_empty() {
                Vec::new()
            } else {
                f[7].split(';').map(unescape).collect()
            },
        })
    }

    /// Every line, oldest first. Refuses (4) a line that cannot be read.
    pub fn read_all(v: &Versioned) -> Result<Vec<MergeLine>> {
        let path = merges_path(v);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Failure::refused(format!("{}: {e}", path.display()))),
        };
        let mut out = Vec::new();
        for (n, line) in text.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            out.push(MergeLine::parse(line).ok_or_else(|| {
                Failure::refused(format!("{}:{}: not a merge line", path.display(), n + 1))
            })?);
        }
        Ok(out)
    }

    /// Appends the line and flushes it to disk.
    pub fn append(&self, v: &Versioned) -> Result<()> {
        let path = merges_path(v);
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ctx(path.display())?;
        f.write_all(self.render().as_bytes()).ctx(path.display())?;
        f.sync_all().ctx(path.display())
    }
}

/// Keeps a path inside the versioned folder for messages.
pub fn display(v: &Versioned, rel: &str) -> String {
    Path::new(v.dir()).join(rel).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_lines_round_trip_with_separators_in_fields() {
        let line = MergeLine {
            time: "2026-11-13T10:00:00Z".into(),
            branch: "disk/api".into(),
            lifetime: "00ff".into(),
            commit: "a".repeat(40),
            old: "b".repeat(40),
            m: "c".repeat(40),
            run: "20261113T1012Z-0011223344556677".into(),
            platforms: vec![
                "macos-aarch64=pass@local".into(),
                "linux-x86_64=waived:owner; said\tso 100%".into(),
            ],
        };
        let text = line.render();
        assert_eq!(text.matches('\t').count(), 7);
        assert_eq!(MergeLine::parse(text.trim_end_matches('\n')).unwrap(), line);
    }
}
