//! The conflict forecast (DESIGN §8.1): `git merge-tree --write-tree` for each open branch
//! against the integration branch and for every pair of open branches. It warns and refuses
//! nothing. `status` prints it and `merge` forecasts only the pairs that involve the batch.
//!
//! No file is touched and no merge driver runs. The objects `merge-tree` writes go to a
//! temporary object folder that reads the repository's objects as an alternate, and the folder
//! is removed afterwards (a folder a killed process left is removed by the next forecast).
//!
//! The answer does not depend on the checkout it runs in: attributes come from the integration
//! branch's tip (`GIT_ATTR_SOURCE`, git 2.40; older git gets the tip's `.gitattributes` files in
//! a temporary work tree), with the gate's configuration. A pair is forecast on its own, so it
//! cannot see attributes that one branch of a batch hands to the next; the gate can.
//!
//! A pair for which the gate would refuse the merge (a merge driver git does not ship) is
//! reported as refused and never given to `merge-tree`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::exit::{Failure, Result};
use crate::fsutil;
use crate::gate::{self, GATE_CONFIG, split_z};
use crate::git::{self, Checkout};
use crate::records::BranchState;
use crate::settings::Settings;
use crate::stack::Stack;

/// The most `merge-tree` calls a whole-project forecast makes; the rest are counted as not
/// forecast. A forecast of a batch's pairs has no limit.
pub const MAX_CALLS: usize = 300;

const WORKERS: usize = 4;

/// The folders of a forecast: `$TMPDIR/atelier-forecast-<pid>-<hex>`.
const SCRATCH_PREFIX: &str = "atelier-forecast-";

/// An open branch: its name, tip and the commit it is built on (empty for the integration
/// branch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Open {
    pub name: String,
    pub tip: String,
    pub base: String,
}

/// Two branches whose merge conflicts, with the files; or whose merge the gate would refuse,
/// with a line per path and driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub left: String,
    pub right: String,
    pub files: Vec<String>,
    /// `files` are `<path> uses merge driver <name>`: the gate refuses this merge.
    pub refused: bool,
}

impl Conflict {
    /// `a x b: file, file (+2 more)`, or `a x b: would be refused: p uses merge driver d`.
    pub fn line(&self, show_files: usize) -> String {
        let shown: Vec<&str> = self
            .files
            .iter()
            .take(show_files)
            .map(String::as_str)
            .collect();
        let more = self.files.len().saturating_sub(show_files);
        format!(
            "{} x {}: {}{}{}",
            self.left,
            self.right,
            if self.refused {
                "would be refused: "
            } else {
                ""
            },
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
    /// The pairs that conflict, or that the gate would refuse.
    pub conflicts: Vec<Conflict>,
    /// Pairs forecast.
    pub pairs: usize,
    /// Pairs not forecast: past [`MAX_CALLS`], git could not tell, or the work of the pair failed.
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
            Some(Open {
                name: r.name,
                tip,
                base: r.base,
            })
        })
        .collect()
}

/// The pairs to forecast: each branch against `main`, then every pair of `branches`; with a
/// `batch`, only those that involve one of its branches. Without a batch the list is cut at
/// [`MAX_CALLS`]; the second value is how many pairs that left out.
fn pairs<'a>(
    main: &'a Open,
    branches: &'a [Open],
    batch: Option<&[String]>,
) -> (Vec<(&'a Open, &'a Open)>, usize) {
    let in_batch = |o: &Open| batch.is_none_or(|b| b.contains(&o.name));
    let mut jobs: Vec<(&Open, &Open)> = Vec::new();
    for (i, a) in branches.iter().enumerate() {
        if in_batch(a) {
            jobs.push((a, main));
        }
        for b in &branches[i + 1..] {
            if in_batch(a) || in_batch(b) {
                jobs.push((a, b));
            }
        }
    }
    let mut skipped = 0;
    if batch.is_none() && jobs.len() > MAX_CALLS {
        skipped = jobs.len() - MAX_CALLS;
        jobs.truncate(MAX_CALLS);
    }
    (jobs, skipped)
}

/// What forecasting one pair found.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Conflicts(Vec<String>),
    Refused(Vec<String>),
    Unknown,
}

/// What every pair shares.
struct Env<'a> {
    root: &'a Path,
    scratch: &'a Scratch,
    /// The integration branch's tip: the source of the attributes.
    attr_tip: &'a str,
    /// OLD's regenerated paths.
    old_regen: BTreeSet<String>,
}

/// Each branch against `main`, then every pair of `branches`; with a `batch`, only the pairs
/// that involve it. `old_settings` are the integration branch's committed settings: a conflict
/// only in a path they name in a `merge.after` hook with a commit, and the branch tip's
/// committed settings name too, is settled by `merge` and not listed. A branch that changes
/// `.atelier/` or the rules files is never filtered: the gate settles nothing for it.
pub fn forecast(
    checkout: &Checkout,
    old_settings: Option<&Settings>,
    main: &Open,
    branches: &[Open],
    batch: Option<&[String]>,
) -> Forecast {
    remove_dead_scratches();
    let (jobs, skipped) = pairs(main, branches, batch);
    let mut out = Forecast {
        unknown: skipped,
        ..Forecast::default()
    };
    if jobs.is_empty() {
        return out;
    }
    let Ok(scratch) = Scratch::new(checkout, &main.tip) else {
        out.unknown += jobs.len();
        return out;
    };
    let old_regen = old_settings
        .map(gate::regenerated_paths)
        .unwrap_or_default();
    let env = Env {
        root: &checkout.root,
        scratch: &scratch,
        attr_tip: &main.tip,
        old_regen,
    };
    // Each branch's own filter, once.
    let filters: Vec<(&str, BTreeSet<String>)> = branches
        .iter()
        .map(|b| (b.name.as_str(), branch_regen(&env, old_settings, b)))
        .collect();
    let regen_of = |o: &Open| -> &BTreeSet<String> {
        if o.name == main.name {
            return &env.old_regen;
        }
        &filters
            .iter()
            .find(|(n, _)| *n == o.name)
            .expect("a branch of the jobs")
            .1
    };
    let results = run_parallel(&jobs, |(a, b)| judge(&env, a, b));
    scratch.remove();
    for ((a, b), r) in jobs.iter().zip(results) {
        match r.unwrap_or(Verdict::Unknown) {
            Verdict::Unknown => out.unknown += 1,
            Verdict::Refused(lines) => {
                out.pairs += 1;
                out.conflicts.push(Conflict {
                    left: a.name.clone(),
                    right: b.name.clone(),
                    files: lines,
                    refused: true,
                });
            }
            Verdict::Conflicts(files) => {
                out.pairs += 1;
                let (ra, rb) = (regen_of(a), regen_of(b));
                let files: Vec<String> = files
                    .into_iter()
                    .filter(|f| !(ra.contains(f) && rb.contains(f)))
                    .collect();
                if !files.is_empty() {
                    out.conflicts.push(Conflict {
                        left: a.name.clone(),
                        right: b.name.clone(),
                        files,
                        refused: false,
                    });
                }
            }
        }
    }
    out
}

/// The paths the gate may settle a conflict in for this branch: none when the branch changes
/// `.atelier/` or the rules, otherwise those OLD's and the tip's committed settings both name.
/// Anything git cannot tell counts as none.
fn branch_regen(env: &Env, old: Option<&Settings>, b: &Open) -> BTreeSet<String> {
    let Some(old) = old else {
        return BTreeSet::new();
    };
    if env.old_regen.is_empty() {
        return BTreeSet::new();
    }
    match gate::changes_rules(env.root, old, &b.base, &b.tip) {
        Ok(false) => {}
        _ => return BTreeSet::new(),
    }
    match gate::settings_at(env.root, &b.tip) {
        Ok(Some(s)) => gate::regenerated_paths(&s)
            .intersection(&env.old_regen)
            .cloned()
            .collect(),
        _ => BTreeSet::new(),
    }
}

/// The gate's read-only merge-driver check, then `merge-tree`.
fn judge(env: &Env, a: &Open, b: &Open) -> Verdict {
    let mut sources: Vec<Option<&str>> = vec![Some(env.attr_tip)];
    for t in [a.tip.as_str(), b.tip.as_str()] {
        if !sources.contains(&Some(t)) {
            sources.push(Some(t));
        }
    }
    match gate::merge_driver_uses(env.root, &a.tip, &b.tip, &sources) {
        Ok(uses) if uses.is_empty() => {}
        Ok(uses) => {
            return Verdict::Refused(
                uses.into_iter()
                    .map(|u| format!("{} uses merge driver {}", u.path, u.driver))
                    .collect(),
            );
        }
        Err(_) => return Verdict::Unknown,
    }
    match merge_tree(env, &a.tip, &b.tip) {
        Some(files) => Verdict::Conflicts(files),
        None => Verdict::Unknown,
    }
}

/// Runs `f` on every item, a quarter of them per thread. An item whose work panicked, or whose
/// thread did, has no result.
fn run_parallel<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<Option<R>> {
    if items.is_empty() {
        return Vec::new();
    }
    let chunk = items.len().div_ceil(WORKERS);
    std::thread::scope(|s| {
        let f = &f;
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|part| {
                let h = s.spawn(move || {
                    part.iter()
                        .map(|item| {
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(item))).ok()
                        })
                        .collect::<Vec<_>>()
                });
                (part.len(), h)
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|(n, h)| h.join().unwrap_or_else(|_| (0..n).map(|_| None).collect()))
            .collect()
    })
}

/// The warning lines of a forecast for `merge`: the conflicts and refusals that involve a branch
/// of the batch, and, whenever some pair was not forecast, how many.
pub fn warning_lines(f: &Forecast, batch: &[String]) -> Vec<String> {
    let mut lines: Vec<String> = f
        .conflicts
        .iter()
        .filter(|c| batch.iter().any(|b| *b == c.left || *b == c.right))
        .map(|c| format!("warning: forecast: {}", c.line(5)))
        .collect();
    if f.unknown > 0 {
        lines.push(format!(
            "warning: forecast: {} pairs not forecast",
            f.unknown
        ));
    }
    lines
}

/// The temporary object folder, and where `merge-tree` reads attributes.
struct Scratch {
    dir: PathBuf,
    alternate: PathBuf,
    attrs: Attrs,
}

enum Attrs {
    /// `GIT_ATTR_SOURCE`: the integration tip.
    Source(String),
    /// A work tree that holds the tip's `.gitattributes` files, for git older than 2.40.
    Tree {
        work: PathBuf,
        git_dir: PathBuf,
        index: PathBuf,
    },
}

impl Scratch {
    fn new(checkout: &Checkout, tip: &str) -> Result<Scratch> {
        Self::new_with(checkout, tip, supports_attr_source(&checkout.root))
    }

    fn new_with(checkout: &Checkout, tip: &str, native: bool) -> Result<Scratch> {
        let dir = std::env::temp_dir().join(format!(
            "{SCRATCH_PREFIX}{}-{}",
            std::process::id(),
            fsutil::random_hex(4).map_err(|e| Failure::refused(e.to_string()))?
        ));
        std::fs::create_dir(&dir)
            .map_err(|e| Failure::refused(format!("{}: {e}", dir.display())))?;
        let mut scratch = Scratch {
            alternate: checkout.common_dir.join("objects"),
            attrs: Attrs::Source(tip.to_owned()),
            dir,
        };
        if !native {
            match attribute_tree(checkout, tip, &scratch.dir) {
                Ok(attrs) => scratch.attrs = attrs,
                Err(e) => {
                    scratch.remove();
                    return Err(e);
                }
            }
        }
        Ok(scratch)
    }

    fn remove(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// git 2.40 reads attributes from a tree (`GIT_ATTR_SOURCE`).
fn supports_attr_source(dir: &Path) -> bool {
    let Ok(v) = git::run(dir, ["--version"]) else {
        return true;
    };
    let nums: Vec<u32> = v
        .split_whitespace()
        .nth(2)
        .unwrap_or("")
        .split('.')
        .map_while(|p| p.parse().ok())
        .collect();
    match nums.as_slice() {
        [major, minor, ..] => (*major, *minor) >= (2, 40),
        _ => true,
    }
}

/// The tip's `.gitattributes` files written to a work tree of their own, with an empty index.
fn attribute_tree(checkout: &Checkout, tip: &str, scratch: &Path) -> Result<Attrs> {
    let work = scratch.join("attrs");
    std::fs::create_dir(&work).map_err(|e| Failure::refused(format!("{}: {e}", work.display())))?;
    let listed = git::output(&checkout.root, ["ls-tree", "-r", "-z", "--name-only", tip])?;
    if !listed.status.success() {
        return Err(Failure::refused("git ls-tree failed"));
    }
    for path in split_z(&listed.stdout) {
        if path.rsplit('/').next() != Some(".gitattributes") {
            continue;
        }
        let blob = git::output(&checkout.root, ["show", &format!("{tip}:{path}")])?;
        if !blob.status.success() {
            return Err(Failure::refused("git show failed"));
        }
        let target = work.join(&path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Failure::refused(format!("{}: {e}", parent.display())))?;
        }
        std::fs::write(&target, &blob.stdout)
            .map_err(|e| Failure::refused(format!("{}: {e}", target.display())))?;
    }
    Ok(Attrs::Tree {
        work,
        git_dir: checkout.common_dir.clone(),
        index: scratch.join("index"),
    })
}

/// Removes the forecast folders of processes that no longer exist (killed during a forecast).
fn remove_dead_scratches() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(SCRATCH_PREFIX)) else {
            continue;
        };
        let Some(pid) = rest
            .split_once('-')
            .and_then(|(pid, _)| pid.parse::<i32>().ok())
        else {
            continue;
        };
        if pid > 1 && pid != std::process::id() as i32 && !process_exists(pid) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

fn process_exists(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// The conflicted files of merging two commits, or `None` when git cannot tell (unrelated
/// histories, a missing object).
fn merge_tree(env: &Env, a: &str, b: &str) -> Option<Vec<String>> {
    let scratch = env.scratch;
    let mut cmd = match &scratch.attrs {
        Attrs::Source(tip) => {
            let mut c = git::command(env.root);
            c.env("GIT_ATTR_SOURCE", tip);
            c
        }
        Attrs::Tree {
            work,
            git_dir,
            index,
        } => {
            let mut c = git::command(work);
            c.env("GIT_DIR", git_dir)
                .env("GIT_WORK_TREE", work)
                .env("GIT_INDEX_FILE", index);
            c
        }
    };
    let out = cmd
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

#[cfg(test)]
mod tests {
    use super::*;

    fn open(name: &str) -> Open {
        Open {
            name: name.to_owned(),
            tip: name.to_owned(),
            base: String::new(),
        }
    }

    fn names(jobs: &[(&Open, &Open)]) -> Vec<String> {
        jobs.iter()
            .map(|(a, b)| format!("{}-{}", a.name, b.name))
            .collect()
    }

    #[test]
    fn a_batch_forecasts_only_its_pairs_and_is_never_cut() {
        let main = open("main");
        let branches: Vec<Open> = ('a'..='z').map(|c| open(&c.to_string())).collect();
        let batch = vec!["z".to_owned()];
        let (jobs, skipped) = pairs(&main, &branches, Some(&batch));
        assert_eq!(skipped, 0);
        // z against main and against each of the 25 others.
        assert_eq!(jobs.len(), 26);
        assert!(names(&jobs).contains(&"z-main".to_owned()));
        assert!(names(&jobs).contains(&"a-z".to_owned()));

        let (all, skipped) = pairs(&main, &branches, None);
        assert_eq!(all.len(), MAX_CALLS);
        assert_eq!(skipped, 26 + 325 - MAX_CALLS);

        let two = vec!["a".to_owned(), "b".to_owned()];
        let (jobs, _) = pairs(&main, &branches, Some(&two));
        // a-main, b-main, a with 25, b with 24 (a-b once).
        assert_eq!(jobs.len(), 2 + 25 + 24);
    }

    #[test]
    fn a_panicking_job_has_no_result_and_the_others_do() {
        let items: Vec<u32> = (0..9).collect();
        let r = run_parallel(&items, |n| {
            if *n == 4 {
                panic!("a job panics");
            }
            n * 2
        });
        assert_eq!(r.len(), 9);
        assert_eq!(r[4], None);
        assert_eq!(r[3], Some(6));
        assert_eq!(r[8], Some(16));
    }

    #[test]
    fn warning_lines_always_count_the_pairs_not_forecast() {
        let f = Forecast {
            unknown: 3,
            ..Forecast::default()
        };
        assert_eq!(
            warning_lines(&f, &["a".to_owned()]),
            ["warning: forecast: 3 pairs not forecast"]
        );
        assert!(warning_lines(&Forecast::default(), &["a".to_owned()]).is_empty());
    }

    #[test]
    fn a_dead_process_scratch_is_removed_and_a_live_one_is_not() {
        let tmp = std::env::temp_dir();
        let dead = {
            let mut c = std::process::Command::new("true").spawn().unwrap();
            let pid = c.id();
            c.wait().unwrap();
            pid
        };
        let gone = tmp.join(format!("{SCRATCH_PREFIX}{dead}-0f0f0f0f"));
        let ours = tmp.join(format!("{SCRATCH_PREFIX}{}-0e0e0e0e", std::process::id()));
        std::fs::create_dir_all(gone.join("x")).unwrap();
        std::fs::create_dir_all(&ours).unwrap();
        remove_dead_scratches();
        assert!(!gone.exists());
        assert!(ours.exists());
        let _ = std::fs::remove_dir_all(ours);
    }

    fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["-c", "user.name=t", "-c", "user.email=t@t.invalid"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    /// The attribute work tree used for git before 2.40 gives the same answer as
    /// `GIT_ATTR_SOURCE`, from any checkout.
    #[test]
    fn the_attribute_tree_fallback_reads_the_tips_attributes_from_any_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        git_in(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join(".gitattributes"), "shared.txt -merge\n").unwrap();
        std::fs::write(root.join("shared.txt"), "one\ntwo\nthree\n").unwrap();
        git_in(&root, &["add", "-A"]);
        git_in(&root, &["commit", "-qm", "init"]);
        git_in(&root, &["checkout", "-qb", "a"]);
        git_in(&root, &["rm", "-q", ".gitattributes"]);
        std::fs::write(root.join("shared.txt"), "ONE\ntwo\nthree\n").unwrap();
        git_in(&root, &["commit", "-qam", "a"]);
        git_in(&root, &["checkout", "-q", "main"]);
        git_in(&root, &["checkout", "-qb", "b"]);
        std::fs::write(root.join("shared.txt"), "one\ntwo\ntres\n").unwrap();
        git_in(&root, &["commit", "-qam", "b"]);
        git_in(&root, &["checkout", "-q", "main"]);
        let wa = root.parent().unwrap().join("wa");
        git_in(&root, &["worktree", "add", "-q", wa.to_str().unwrap(), "a"]);
        let tip = git_in(&root, &["rev-parse", "main"]);
        let (a, b) = (
            git_in(&root, &["rev-parse", "a"]),
            git_in(&root, &["rev-parse", "b"]),
        );
        for dir in [&root, &wa] {
            let checkout = Checkout::find(dir).unwrap().unwrap();
            for native in [true, false] {
                let scratch = Scratch::new_with(&checkout, &tip, native).unwrap();
                let env = Env {
                    root: &checkout.root,
                    scratch: &scratch,
                    attr_tip: &tip,
                    old_regen: BTreeSet::new(),
                };
                // -merge at the tip: a binary merge conflicts, from either checkout (a's
                // worktree has no attribute file, where a text merge would be clean).
                assert_eq!(
                    merge_tree(&env, &a, &b),
                    Some(vec!["shared.txt".to_owned()]),
                    "{dir:?} {native}"
                );
                scratch.remove();
            }
        }
    }
}
