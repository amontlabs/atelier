//! Branch names and the file keys made from them (DESIGN §3 intro).
//!
//! A branch name is any name `git check-ref-format --branch` accepts, `/` included. atelier never
//! uses a name as a path: lock files, registry entries and temp folders are keyed by the name
//! percent-encoded into one path segment ([`key`]).

use std::path::Path;

use crate::exit::{Failure, Result};
use crate::git;

/// Longest key accepted: one path segment, with room for a suffix such as `.lock`.
pub const MAX_KEY_LEN: usize = 200;

/// Checks a branch name with git and returns it unchanged. Refuses (2, wrong usage) any name git
/// rejects, any name git would expand into another (`@{-1}`), and names whose key is too long.
pub fn validate_branch(dir: &Path, name: &str) -> Result<String> {
    if name.is_empty() || name.starts_with('-') {
        return Err(Failure::usage(format!("invalid branch name: {name:?}")));
    }
    let out = git::output(dir, ["check-ref-format", "--branch", name])?;
    let printed = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || printed.trim_end_matches('\n') != name {
        return Err(Failure::usage(format!("invalid branch name: {name:?}")));
    }
    if key(name).len() > MAX_KEY_LEN {
        return Err(Failure::usage(format!(
            "branch name too long for atelier ({} bytes once encoded, at most {MAX_KEY_LEN}): {name:?}",
            key(name).len()
        )));
    }
    Ok(name.to_owned())
}

/// The full ref of a branch name.
pub fn branch_ref(name: &str) -> String {
    format!("refs/heads/{name}")
}

/// Encodes a name into one path segment. Kept: lowercase ASCII letters, digits, `-`, `_`, and `.`
/// when not first. Every other byte, uppercase letters included, becomes `%XX` (uppercase hex).
///
/// The encoding is injective, never contains `/`, is never `.` or `..`, and stays injective when a
/// case-insensitive file system folds case (an uppercase letter only ever appears inside an
/// escape), so `a/b`, `a%2Fb`, `A` and `a` give four different files on every file system.
pub fn key(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, b) in name.bytes().enumerate() {
        let keep = b.is_ascii_lowercase()
            || b.is_ascii_digit()
            || b == b'-'
            || b == b'_'
            || (b == b'.' && i > 0);
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decodes a key made by [`key`]. `None` if it is not one.
pub fn unkey(k: &str) -> Option<String> {
    let bytes = k.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = k.get(i + 1..i + 3)?;
            if hex.bytes().any(|c| c.is_ascii_lowercase()) {
                return None;
            }
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let s = String::from_utf8(out).ok()?;
    (key(&s) == k).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_never_alias() {
        let names = [
            "a/b", "a%2Fb", "a%2fb", "A", "a", "Foo", "foo", "FOO", "a.b", "é", "e", "@", "x-y_z",
        ];
        let mut seen = std::collections::HashSet::new();
        for n in names {
            let k = key(n);
            assert!(!k.contains('/'), "{k}");
            assert!(k != "." && k != "..", "{k}");
            assert!(
                seen.insert(k.to_lowercase()),
                "case-folded alias for {n}: {k}"
            );
            assert_eq!(unkey(&k).as_deref(), Some(n));
        }
        assert_eq!(key("a/b"), "a%2Fb");
        assert_eq!(key("a%2Fb"), "a%252%46b");
        assert_eq!(key("Foo"), "%46oo");
        assert_eq!(key(".x"), "%2Ex");
        assert_eq!(key("x.y"), "x.y");
    }

    #[test]
    fn unkey_rejects_strings_key_would_not_produce() {
        assert_eq!(unkey("a%2fb"), None);
        assert_eq!(unkey("a/b"), None);
        assert_eq!(unkey("A"), None);
        assert_eq!(unkey("%2"), None);
    }
}
