//! Refs are never followed (r4 §3.1, REV2 1).
//!
//! Every ref atelier reads, records, moves or deletes must be a plain ref: `git symbolic-ref -q`
//! must fail. Every `git update-ref` carries `--no-deref` and the expected old value, so even a
//! ref that turned symbolic after the check is acted on itself, never the ref it points to.

use std::path::Path;

use crate::exit::{Failure, Result};
use crate::git;

/// What a ref must hold for an update to happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expected {
    /// The ref must not exist (creation).
    Absent,
    /// The ref must hold exactly this object id.
    Value(String),
}

/// Refuses (4) a symbolic ref. A missing ref is plain.
pub fn ensure_plain(dir: &Path, refname: &str) -> Result<()> {
    check_refname(refname)?;
    let out = git::output(dir, ["symbolic-ref", "-q", refname])?;
    match out.status.code() {
        Some(1) => Ok(()),
        Some(0) => Err(Failure::refused(format!(
            "{refname} is a symbolic ref (an alias of {}): atelier uses plain refs only",
            String::from_utf8_lossy(&out.stdout).trim()
        ))),
        _ => Err(Failure::refused(format!(
            "cannot inspect {refname}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))),
    }
}

/// Reads a plain ref's object id; `None` if the ref does not exist. Refuses (4) a symbolic ref,
/// checked twice: by `git symbolic-ref`, and in the same read that returns the value.
pub fn read(dir: &Path, refname: &str) -> Result<Option<String>> {
    ensure_plain(dir, refname)?;
    let text = git::run(
        dir,
        [
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(symref)",
            refname,
        ],
    )?;
    for line in text.lines() {
        let mut parts = line.split('\0');
        let (Some(name), Some(oid), Some(symref)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if name != refname {
            continue;
        }
        if !symref.is_empty() {
            return Err(Failure::refused(format!(
                "{refname} is a symbolic ref (an alias of {symref}): atelier uses plain refs only"
            )));
        }
        return Ok(Some(oid.to_owned()));
    }
    Ok(None)
}

/// Moves (or creates) a plain ref to `new`, only if it holds `expected`. Refuses (4) a symbolic
/// ref and a ref that moved.
pub fn update(
    dir: &Path,
    refname: &str,
    new: &str,
    expected: &Expected,
    reason: &str,
) -> Result<()> {
    ensure_plain(dir, refname)?;
    check_oid(new)?;
    let old = expected_arg(dir, expected)?;
    let out = git::output(
        dir,
        ["update-ref", "--no-deref", "-m", reason, refname, new, &old],
    )?;
    if !out.status.success() {
        return Err(moved(refname, expected, &out.stderr));
    }
    Ok(())
}

/// Deletes a plain ref, only if it holds `expected`. Refuses (4) a symbolic ref and a ref that
/// moved.
pub fn delete(dir: &Path, refname: &str, expected: &str) -> Result<()> {
    ensure_plain(dir, refname)?;
    check_oid(expected)?;
    let out = git::output(dir, ["update-ref", "--no-deref", "-d", refname, expected])?;
    if !out.status.success() {
        return Err(moved(
            refname,
            &Expected::Value(expected.to_owned()),
            &out.stderr,
        ));
    }
    Ok(())
}

fn expected_arg(dir: &Path, expected: &Expected) -> Result<String> {
    match expected {
        Expected::Absent => {
            let checkout = git::Checkout::find(dir)?.ok_or_else(|| {
                Failure::refused(format!("{} is not in a git worktree", dir.display()))
            })?;
            checkout.zero_oid()
        }
        Expected::Value(oid) => {
            check_oid(oid)?;
            Ok(oid.clone())
        }
    }
}

fn moved(refname: &str, expected: &Expected, stderr: &[u8]) -> Failure {
    let want = match expected {
        Expected::Absent => "to be absent".to_owned(),
        Expected::Value(v) => format!("at {v}"),
    };
    Failure::refused(format!(
        "{refname} moved (expected {want}): {}",
        String::from_utf8_lossy(stderr).trim()
    ))
}

/// Only full refs under `refs/` are handled; `HEAD` and abbreviations never are.
fn check_refname(refname: &str) -> Result<()> {
    if !refname.starts_with("refs/") || refname.starts_with('-') {
        return Err(Failure::usage(format!("not a full ref name: {refname:?}")));
    }
    Ok(())
}

/// A full object id: 40 or 64 lowercase hex digits.
pub fn check_oid(oid: &str) -> Result<()> {
    let ok = (oid.len() == 40 || oid.len() == 64)
        && oid
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if ok {
        Ok(())
    } else {
        Err(Failure::usage(format!("not a full object id: {oid:?}")))
    }
}
