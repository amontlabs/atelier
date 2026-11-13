//! The source this program was built from, embedded so that `atelier machines init` can build
//! the same program on another machine (DESIGN §2.5, r4 §4.4: the same program runs on both
//! sides).
//!
//! Why build there from this source: copying a binary needs a cross linker and target libraries
//! for each machine's platform on the Mac, which a plain Rust install lacks; installing with
//! `cargo install --git … --tag` needs the machine to reach a published tag. The embedded files
//! are exactly the ones compiled into this binary, built there with `--locked` against the
//! embedded `Cargo.lock` and the pinned Rust of `rust-toolchain.toml`. The machine still fetches
//! the crates from crates.io (or its own cargo mirror).
//!
//! [`hash`] identifies the source: two binaries of one version built from different source (a
//! development build) are told apart, so a remote run does not use a stale program by mistake.
//! It is what the installed program says about itself, not a check of its integrity: code run
//! as the account that owns the install could replace it (docs/MACHINES.md, same-user trust).

/// One embedded file: its path in the source tree and its bytes.
macro_rules! src_file {
    ($path:literal, $rel:literal) => {
        ($path, include_bytes!($rel) as &[u8])
    };
}

/// Every file `cargo install --path` needs. A test keeps it equal to the tracked source.
pub const FILES: &[(&str, &[u8])] = &[
    ("Cargo.toml", include_bytes!("../Cargo.toml")),
    ("Cargo.lock", include_bytes!("../Cargo.lock")),
    (
        "rust-toolchain.toml",
        include_bytes!("../rust-toolchain.toml"),
    ),
    (
        "assets/rules/ORCHESTRATOR.md",
        include_bytes!("../assets/rules/ORCHESTRATOR.md"),
    ),
    (
        "assets/rules/WORKER.md",
        include_bytes!("../assets/rules/WORKER.md"),
    ),
    (
        "assets/rules/agents-block.md",
        include_bytes!("../assets/rules/agents-block.md"),
    ),
    (
        "assets/tasks/build.md",
        include_bytes!("../assets/tasks/build.md"),
    ),
    (
        "assets/tasks/gui.md",
        include_bytes!("../assets/tasks/gui.md"),
    ),
    (
        "assets/tasks/judge.md",
        include_bytes!("../assets/tasks/judge.md"),
    ),
    (
        "assets/tasks/review-fixes.md",
        include_bytes!("../assets/tasks/review-fixes.md"),
    ),
    (
        "assets/tasks/review.md",
        include_bytes!("../assets/tasks/review.md"),
    ),
    (
        "assets/tasks/scout.md",
        include_bytes!("../assets/tasks/scout.md"),
    ),
    src_file!("src/agents.rs", "agents.rs"),
    src_file!("src/approve.rs", "approve.rs"),
    src_file!("src/bisect.rs", "bisect.rs"),
    src_file!("src/check.rs", "check.rs"),
    src_file!("src/clean.rs", "clean.rs"),
    src_file!("src/cli.rs", "cli.rs"),
    src_file!("src/dirfd.rs", "dirfd.rs"),
    src_file!("src/dispatch.rs", "dispatch.rs"),
    src_file!("src/exchange.rs", "exchange.rs"),
    src_file!("src/exit.rs", "exit.rs"),
    src_file!("src/follow.rs", "follow.rs"),
    src_file!("src/fsutil.rs", "fsutil.rs"),
    src_file!("src/gate.rs", "gate.rs"),
    src_file!("src/gate_machines.rs", "gate_machines.rs"),
    src_file!("src/git.rs", "git.rs"),
    src_file!("src/jobs.rs", "jobs.rs"),
    src_file!("src/ledger.rs", "ledger.rs"),
    src_file!("src/lib.rs", "lib.rs"),
    src_file!("src/locks.rs", "locks.rs"),
    src_file!("src/machines.rs", "machines.rs"),
    src_file!("src/main.rs", "main.rs"),
    src_file!("src/names.rs", "names.rs"),
    src_file!("src/nextest.rs", "nextest.rs"),
    src_file!("src/platform.rs", "platform.rs"),
    src_file!("src/preflight.rs", "preflight.rs"),
    src_file!("src/prereq.rs", "prereq.rs"),
    src_file!("src/procs.rs", "procs.rs"),
    src_file!("src/promote.rs", "promote.rs"),
    src_file!("src/records.rs", "records.rs"),
    src_file!("src/refs.rs", "refs.rs"),
    src_file!("src/remote.rs", "remote.rs"),
    src_file!("src/remote_run.rs", "remote_run.rs"),
    src_file!("src/rules.rs", "rules.rs"),
    src_file!("src/runs.rs", "runs.rs"),
    src_file!("src/settings.rs", "settings.rs"),
    src_file!("src/signals.rs", "signals.rs"),
    src_file!("src/source.rs", "source.rs"),
    src_file!("src/stack.rs", "stack.rs"),
    src_file!("src/start.rs", "start.rs"),
    src_file!("src/state.rs", "state.rs"),
    src_file!("src/task.rs", "task.rs"),
    src_file!("src/template.rs", "template.rs"),
    src_file!("src/testcmd.rs", "testcmd.rs"),
    src_file!("src/tmp.rs", "tmp.rs"),
    src_file!("src/tree.rs", "tree.rs"),
];

/// FNV-1a (64 bits) over every file's path, length and bytes, as 16 hex digits.
pub fn hash() -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for (path, bytes) in FILES {
        eat(path.as_bytes());
        eat(&[0]);
        eat(&(bytes.len() as u64).to_le_bytes());
        eat(bytes);
    }
    format!("{h:016x}")
}

/// `<version> <source hash>`: what `remote-run identity` prints.
pub fn identity() -> String {
    format!("{} {}", crate::dispatch::VERSION, hash())
}

/// The embedded files as a POSIX ustar archive, for `tar -x` on the machine.
pub fn tar() -> Vec<u8> {
    let mut out = Vec::new();
    for (path, bytes) in FILES {
        out.extend_from_slice(&header(path, bytes.len()));
        out.extend_from_slice(bytes);
        let pad = (512 - bytes.len() % 512) % 512;
        out.extend(std::iter::repeat_n(0u8, pad));
    }
    out.extend(std::iter::repeat_n(0u8, 1024));
    out
}

fn header(path: &str, size: usize) -> [u8; 512] {
    assert!(path.len() < 100, "{path}: too long for a ustar name");
    let mut h = [0u8; 512];
    let mut put = |at: usize, s: &[u8]| h[at..at + s.len()].copy_from_slice(s);
    put(0, path.as_bytes());
    put(100, b"0000644\0");
    put(108, b"0000000\0");
    put(116, b"0000000\0");
    put(124, format!("{size:011o}\0").as_bytes());
    put(136, b"00000000000\0");
    put(148, b"        ");
    put(156, b"0");
    put(257, b"ustar\0");
    put(263, b"00");
    let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    #[test]
    fn the_embedded_files_are_the_tracked_source() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let out = Command::new("git")
            .current_dir(root)
            .args([
                "ls-files",
                "--",
                "src",
                "assets",
                "Cargo.toml",
                "Cargo.lock",
                "rust-toolchain.toml",
            ])
            .output()
            .unwrap();
        assert!(out.status.success());
        let mut tracked: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        // A file added to src/ but not committed yet still counts.
        for e in std::fs::read_dir(root.join("src")).unwrap() {
            let name = format!("src/{}", e.unwrap().file_name().to_string_lossy());
            if !tracked.contains(&name) {
                tracked.push(name);
            }
        }
        tracked.sort();
        let mut embedded: Vec<String> = FILES.iter().map(|(p, _)| p.to_string()).collect();
        embedded.sort();
        assert_eq!(embedded, tracked, "update source::FILES");
        for (p, bytes) in FILES {
            assert_eq!(&std::fs::read(root.join(p)).unwrap(), bytes, "{p}");
        }
    }

    #[test]
    fn the_archive_unpacks_with_tar_to_the_same_files() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.tar");
        std::fs::write(&archive, tar()).unwrap();
        let out_dir = dir.path().join("out");
        std::fs::create_dir(&out_dir).unwrap();
        let st = Command::new("tar")
            .arg("-xf")
            .arg(&archive)
            .arg("-C")
            .arg(&out_dir)
            .status()
            .unwrap();
        assert!(st.success());
        for (p, bytes) in FILES {
            assert_eq!(&std::fs::read(out_dir.join(p)).unwrap(), bytes, "{p}");
        }
    }

    #[test]
    fn the_identity_names_the_version_and_a_stable_hash() {
        assert_eq!(hash(), hash());
        assert_eq!(hash().len(), 16);
        assert!(identity().starts_with(crate::dispatch::VERSION));
    }
}
