//! The conflict forecast (DESIGN §8.1): `git merge-tree --write-tree` for each open branch
//! against the integration branch and for every pair of open branches. It warns and refuses
//! nothing. `status` prints it and `merge` prints the pairs that involve the batch.
//!
//! No file is touched: the objects `merge-tree` writes go to a temporary object folder that
//! reads the repository's objects as an alternate, and the folder is removed afterwards.

use std::collections::BTreeSet;
use std::path::Path;

use crate::exit::{Failure, Result};
use crate::fsutil;
use crate::gate::{GATE_CONFIG, split_z};
use crate::git::{self, Checkout};
use crate::records::BranchState;
use crate::stack::Stack;

/// The most `merge-tree` calls one forecast makes; the rest are counted as not forecast.
pub const MAX_CALLS: usize = 300;

const WORKERS: usize = 4;

/// An open branch: its name and tip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Open {
    pub name: String,
    pub tip: String,
}

/// Two branches whose merge conflicts, with the files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub left: String,
    pub right: String,
    pub files: Vec<String>,
}

impl Conflict {
    /// `a x b: file, file (+2 more)`.
    pub fn line(&self, show_files: usize) -> String {
        let shown: Vec<&str> = self
            .files
            .iter()
            .take(show_files)
            .map(String::as_str)
            .collect();
        let more = self.files.len().saturating_sub(show_files);
        format!(
            "{} x {}: {}{}",
            self.left,
            self.right,
            shown.join(", "),
            if more > 0 {
                format!(" (+{more} more)")
            } else {
                String::new()
            }
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Forecast {
    pub conflicts: Vec<Conflict>,
    /// Pairs forecast.
    pub pairs: usize,
    /// Pairs not forecast: past [`MAX_CALLS`], or git could not tell.
    pub unknown: usize,
}

/// The open branches: active records whose start finished and whose branch exists.
pub fn open_branches(stack: &Stack) -> Vec<Open> {
    let (records, _) = stack.all_records_lenient();
    records
        .into_iter()
        .filter(|r| r.state == BranchState::Active && r.starting.is_none())
        .filter(|r| r.name != stack.integration())
        .filter_map(|r| {
            let tip = stack.tip(&r.name).ok().flatten()?;
            Some(Open { name: r.name, tip })
        })
        .collect()
}

/// Each branch against `main`, then every pair of `branches`. `regen` are the paths the merge's
/// after-hooks regenerate: a conflict only in them is settled by `merge` and not listed.
pub fn forecast(
    checkout: &Checkout,
    main: &Open,
    branches: &[Open],
    regen: &BTreeSet<String>,
) -> Forecast {
    let mut jobs: Vec<(&Open, &Open)> = Vec::new();
    for (i, a) in branches.iter().enumerate() {
        jobs.push((a, main));
        for b in &branches[i + 1..] {
            jobs.push((a, b));
        }
    }
    let mut out = Forecast::default();
    if jobs.len() > MAX_CALLS {
        out.unknown = jobs.len() - MAX_CALLS;
        jobs.truncate(MAX_CALLS);
    }
    if jobs.is_empty() {
        return out;
    }
    let Ok(scratch) = Scratch::new(checkout) else {
        out.unknown += jobs.len();
        return out;
    };
    let results: Vec<Option<Vec<String>>> = std::thread::scope(|s| {
        let chunk = jobs.len().div_ceil(WORKERS);
        let handles: Vec<_> = jobs
            .chunks(chunk)
            .map(|part| {
                let scratch = &scratch;
                s.spawn(move || {
                    part.iter()
                        .map(|(a, b)| merge_tree(&checkout.root, scratch, &a.tip, &b.tip))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    scratch.remove();
    for ((a, b), r) in jobs.iter().zip(results) {
        match r {
            None => out.unknown += 1,
            Some(files) => {
                out.pairs += 1;
                let files: Vec<String> = files.into_iter().filter(|f| !regen.contains(f)).collect();
                if !files.is_empty() {
                    out.conflicts.push(Conflict {
                        left: a.name.clone(),
                        right: b.name.clone(),
                        files,
                    });
                }
            }
        }
    }
    out
}

/// The warning lines of a forecast for `merge`: the conflicts that involve a branch of the batch.
pub fn warning_lines(f: &Forecast, batch: &[String]) -> Vec<String> {
    let mut lines: Vec<String> = f
        .conflicts
        .iter()
        .filter(|c| batch.iter().any(|b| *b == c.left || *b == c.right))
        .map(|c| format!("warning: forecast: {}", c.line(5)))
        .collect();
    if f.unknown > 0 && !lines.is_empty() {
        lines.push(format!(
            "warning: forecast: {} pairs not forecast",
            f.unknown
        ));
    }
    lines
}

/// The temporary object folder.
struct Scratch {
    dir: std::path::PathBuf,
    alternate: std::path::PathBuf,
}

impl Scratch {
    fn new(checkout: &Checkout) -> Result<Scratch> {
        let dir = std::env::temp_dir().join(format!(
            "atelier-forecast-{}-{}",
            std::process::id(),
            fsutil::random_hex(4).map_err(|e| Failure::refused(e.to_string()))?
        ));
        std::fs::create_dir(&dir)
            .map_err(|e| Failure::refused(format!("{}: {e}", dir.display())))?;
        Ok(Scratch {
            dir,
            alternate: checkout.common_dir.join("objects"),
        })
    }

    fn remove(self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The conflicted files of merging two commits, or `None` when git cannot tell (unrelated
/// histories, a missing object).
fn merge_tree(dir: &Path, scratch: &Scratch, a: &str, b: &str) -> Option<Vec<String>> {
    let out = git::command(dir)
        .args(GATE_CONFIG)
        .args([
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            "-z",
            a,
            b,
        ])
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_OBJECT_DIRECTORY", &scratch.dir)
        .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &scratch.alternate)
        .output()
        .ok()?;
    match out.status.code() {
        Some(0) => Some(Vec::new()),
        Some(1) => {
            let files: BTreeSet<String> = split_z(&out.stdout).into_iter().skip(1).collect();
            Some(files.into_iter().collect())
        }
        _ => None,
    }
}
