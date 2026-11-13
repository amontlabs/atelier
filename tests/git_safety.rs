//! Plain refs, branch names, the tree id and the submodule refusal, on throwaway repositories.

mod common;

use std::fs;

use atelier::exit::Exit;
use atelier::names;
use atelier::refs::{self, Expected};
use atelier::tree;
use common::*;

#[test]
fn branch_names_follow_git_and_never_alias_as_keys() {
    let repo = Repo::new();
    for ok in ["a/b", "a%2Fb", "Foo", "foo", "disk-ui", "é", "@", "x.y"] {
        assert_eq!(names::validate_branch(&repo.root, ok).unwrap(), ok);
    }
    for bad in [
        "a//b", "a/./b", "a/", "a.lock/b", "..", "a..b", "../x", "x/../y", ".x", "-x", "HEAD",
        "a b", "a@{b", "", "a~1", "a^", "a:b", "a\\b", "a*", "a?", "a[",
    ] {
        let e = names::validate_branch(&repo.root, bad).unwrap_err();
        assert_eq!(e.exit, Exit::Usage, "{bad:?}");
    }
    // `@{-1}` would be expanded by git into the previous branch's name: refused, not expanded.
    git(&repo.root, &["switch", "-q", "-c", "other"]);
    git(&repo.root, &["switch", "-q", "main"]);
    assert!(names::validate_branch(&repo.root, "@{-1}").is_err());
    // A name too long once encoded.
    assert!(names::validate_branch(&repo.root, &"A".repeat(80)).is_err());

    let keys: Vec<String> = ["a/b", "a%2Fb", "Foo", "foo"]
        .iter()
        .map(|n| names::key(n))
        .collect();
    for k in &keys {
        assert!(!k.contains('/'));
    }
    let mut folded: Vec<String> = keys.iter().map(|k| k.to_lowercase()).collect();
    folded.sort();
    folded.dedup();
    assert_eq!(folded.len(), 4);
}

#[test]
fn symbolic_refs_are_never_read_moved_or_deleted() {
    let repo = Repo::new();
    let main = git(&repo.root, &["rev-parse", "refs/heads/main"]);
    git(
        &repo.root,
        &["symbolic-ref", "refs/heads/alias", "refs/heads/main"],
    );

    let e = refs::read(&repo.root, "refs/heads/alias").unwrap_err();
    assert_eq!(e.exit, Exit::Refused);
    assert!(e.message.contains("symbolic ref"), "{}", e.message);
    assert_eq!(
        refs::read(&repo.root, "refs/heads/main").unwrap(),
        Some(main.clone())
    );
    assert_eq!(refs::read(&repo.root, "refs/heads/missing").unwrap(), None);

    repo.write("f", "x");
    let next = repo.commit_all("next");
    git(&repo.root, &["update-ref", "refs/heads/main", &main]);
    git(&repo.root, &["reset", "-q", "--hard", &main]);

    let e = refs::update(
        &repo.root,
        "refs/heads/alias",
        &next,
        &Expected::Value(main.clone()),
        "test",
    )
    .unwrap_err();
    assert_eq!(e.exit, Exit::Refused);
    let e = refs::delete(&repo.root, "refs/heads/alias", &main).unwrap_err();
    assert_eq!(e.exit, Exit::Refused);
    // main did not move and was not deleted through its alias.
    assert_eq!(git(&repo.root, &["rev-parse", "refs/heads/main"]), main);
    assert_eq!(
        git(&repo.root, &["symbolic-ref", "refs/heads/alias"]),
        "refs/heads/main"
    );
}

#[test]
fn update_ref_uses_no_deref_even_if_a_ref_turns_symbolic_after_the_check() {
    // The race of REV2 1: the check passed, then the ref became an alias. `--no-deref` with an
    // expected old value means git refuses rather than moving the ref the alias points to.
    let repo = Repo::new();
    let main = git(&repo.root, &["rev-parse", "HEAD"]);
    repo.write("f", "x");
    let next = repo.commit_all("next");
    git(&repo.root, &["update-ref", "refs/heads/main", &main]);
    git(&repo.root, &["reset", "-q", "--hard", &main]);
    git(
        &repo.root,
        &["symbolic-ref", "refs/heads/alias", "refs/heads/main"],
    );
    // The exact commands refs::update and refs::delete run, without their plain check.
    let out = git_cmd(&repo.root)
        .args([
            "update-ref",
            "--no-deref",
            "-m",
            "t",
            "refs/heads/alias",
            &next,
            &main,
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    // git acted on the alias itself, which is now a plain ref; main did not move.
    assert_eq!(git(&repo.root, &["rev-parse", "refs/heads/main"]), main);
    assert_eq!(
        refs::read(&repo.root, "refs/heads/alias").unwrap(),
        Some(next.clone())
    );

    git(
        &repo.root,
        &["symbolic-ref", "refs/heads/alias2", "refs/heads/main"],
    );
    let out = git_cmd(&repo.root)
        .args(["update-ref", "--no-deref", "-d", "refs/heads/alias2", &main])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(git(&repo.root, &["rev-parse", "refs/heads/main"]), main);

    // Control: without --no-deref the same update moves main through the alias.
    git(
        &repo.root,
        &["symbolic-ref", "refs/heads/alias3", "refs/heads/main"],
    );
    git(
        &repo.root,
        &["update-ref", "-m", "t", "refs/heads/alias3", &next, &main],
    );
    assert_eq!(git(&repo.root, &["rev-parse", "refs/heads/main"]), next);
}

#[test]
fn updates_need_the_expected_old_value() {
    let repo = Repo::new();
    let first = git(&repo.root, &["rev-parse", "HEAD"]);
    repo.write("f", "x");
    let second = repo.commit_all("second");

    refs::update(
        &repo.root,
        "refs/heads/topic",
        &first,
        &Expected::Absent,
        "create",
    )
    .unwrap();
    assert_eq!(
        refs::read(&repo.root, "refs/heads/topic").unwrap(),
        Some(first.clone())
    );
    // Creating again: it exists.
    let e = refs::update(
        &repo.root,
        "refs/heads/topic",
        &second,
        &Expected::Absent,
        "create",
    )
    .unwrap_err();
    assert_eq!(e.exit, Exit::Refused);
    assert!(e.message.contains("moved"), "{}", e.message);
    // Moving from a wrong old value.
    let e = refs::update(
        &repo.root,
        "refs/heads/topic",
        &second,
        &Expected::Value(second.clone()),
        "move",
    )
    .unwrap_err();
    assert_eq!(e.exit, Exit::Refused);
    assert_eq!(
        refs::read(&repo.root, "refs/heads/topic").unwrap(),
        Some(first.clone())
    );
    refs::update(
        &repo.root,
        "refs/heads/topic",
        &second,
        &Expected::Value(first.clone()),
        "move",
    )
    .unwrap();
    // Deleting from a wrong old value, then the right one.
    assert!(refs::delete(&repo.root, "refs/heads/topic", &first).is_err());
    refs::delete(&repo.root, "refs/heads/topic", &second).unwrap();
    assert_eq!(refs::read(&repo.root, "refs/heads/topic").unwrap(), None);
    // Only full refs and full ids.
    assert_eq!(
        refs::read(&repo.root, "topic").unwrap_err().exit,
        Exit::Usage
    );
    assert_eq!(
        refs::read(&repo.root, "HEAD").unwrap_err().exit,
        Exit::Usage
    );
    assert_eq!(
        refs::update(&repo.root, "refs/heads/x", "HEAD", &Expected::Absent, "m")
            .unwrap_err()
            .exit,
        Exit::Usage
    );
}

#[test]
fn reading_a_ref_does_not_match_its_prefix() {
    let repo = Repo::new();
    let head = git(&repo.root, &["rev-parse", "HEAD"]);
    git(&repo.root, &["update-ref", "refs/heads/a/b", &head]);
    assert_eq!(refs::read(&repo.root, "refs/heads/a").unwrap(), None);
    assert_eq!(
        refs::read(&repo.root, "refs/heads/a/b").unwrap(),
        Some(head)
    );
}

#[test]
fn tree_id_covers_every_non_ignored_file_and_leaves_the_index_alone() {
    let repo = Repo::new();
    repo.write(".gitignore", "target/\n*.log\n");
    repo.write("tracked.log", "tracked even though ignored\n");
    git(&repo.root, &["add", "-f", "tracked.log", ".gitignore"]);
    repo.commit_all("ignore");
    let c = repo.checkout();
    let status_before = git(&repo.root, &["status", "--porcelain"]);
    let index_before = fs::read(repo.root.join(".git/index")).unwrap();

    let clean = tree::tree_id(&c).unwrap();
    assert_eq!(
        Some(clean.clone()),
        tree::head_tree(&c).unwrap(),
        "clean tree = HEAD's tree"
    );

    // Ignored files do not count.
    repo.write("target/out.bin", "build output");
    repo.write("x.log", "log");
    assert_eq!(tree::tree_id(&c).unwrap(), clean);

    // An untracked file counts, by content.
    repo.write("new.txt", "one");
    let with_new = tree::tree_id(&c).unwrap();
    assert_ne!(with_new, clean);
    repo.write("new.txt", "two");
    let changed = tree::tree_id(&c).unwrap();
    assert_ne!(changed, with_new);

    // A tracked file that matches an ignore pattern counts.
    fs::remove_file(repo.root.join("new.txt")).unwrap();
    repo.write("tracked.log", "edited");
    assert_ne!(tree::tree_id(&c).unwrap(), clean);
    repo.write("tracked.log", "tracked even though ignored\n");
    assert_eq!(tree::tree_id(&c).unwrap(), clean);

    // A deleted tracked file counts.
    fs::remove_file(repo.root.join("README")).unwrap();
    assert_ne!(tree::tree_id(&c).unwrap(), clean);
    repo.write("README", "hello\n");

    // The real index and status are unchanged; no temporary index is left.
    assert_eq!(
        fs::read(repo.root.join(".git/index")).unwrap(),
        index_before
    );
    assert_eq!(git(&repo.root, &["status", "--porcelain"]), status_before);
    let leftovers: Vec<_> = fs::read_dir(repo.root.join(".git"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("atelier-tree-index")
        })
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn tree_id_works_in_a_linked_worktree_and_on_an_unborn_branch() {
    let repo = Repo::new();
    let wt = repo.tmp.path().canonicalize().unwrap().join("wt");
    git(
        &repo.root,
        &["worktree", "add", "-q", "-b", "w", wt.to_str().unwrap()],
    );
    let c = atelier::git::Checkout::find(&wt).unwrap().unwrap();
    assert!(!c.is_main_worktree());
    assert_eq!(
        Some(tree::tree_id(&c).unwrap()),
        tree::head_tree(&c).unwrap()
    );

    let fresh = repo.tmp.path().canonicalize().unwrap().join("fresh");
    fs::create_dir(&fresh).unwrap();
    git(&fresh, &["init", "-q", "-b", "main"]);
    fs::write(fresh.join("a"), "a").unwrap();
    let c = atelier::git::Checkout::find(&fresh).unwrap().unwrap();
    assert!(tree::tree_id(&c).is_ok());
    assert_eq!(tree::head_tree(&c).unwrap(), None);
}

#[test]
fn a_committed_submodule_is_refused() {
    let repo = Repo::new();
    let sub = repo.tmp.path().canonicalize().unwrap().join("sub");
    fs::create_dir(&sub).unwrap();
    git(&sub, &["init", "-q", "-b", "main"]);
    fs::write(sub.join("s"), "s").unwrap();
    git(&sub, &["add", "s"]);
    git(&sub, &["commit", "-q", "-m", "s"]);
    git(
        &repo.root,
        &[
            "submodule",
            "add",
            "-q",
            sub.to_str().unwrap(),
            "vendor/sub",
        ],
    );
    repo.commit_all("add submodule");
    let e = tree::tree_id(&repo.checkout()).unwrap_err();
    assert_eq!(e.exit, Exit::Refused);
    assert!(e.message.contains("vendor/sub"), "{}", e.message);
}

#[test]
fn a_gitlink_entry_without_gitmodules_is_refused() {
    let repo = Repo::new();
    let head = git(&repo.root, &["rev-parse", "HEAD"]);
    git(
        &repo.root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{head},linked"),
        ],
    );
    git(&repo.root, &["commit", "-q", "-m", "gitlink"]);
    // No folder at the path: the worktree's own tree would drop the entry, but HEAD has it.
    let e = tree::tree_id(&repo.checkout()).unwrap_err();
    assert!(e.message.contains("linked"), "{}", e.message);
    // An empty folder at the path (an uninitialised submodule): the entry stays.
    fs::create_dir(repo.root.join("linked")).unwrap();
    let e = tree::tree_id(&repo.checkout()).unwrap_err();
    assert!(e.message.contains("linked"), "{}", e.message);
}

#[test]
fn a_nested_repository_that_is_not_ignored_is_refused() {
    let repo = Repo::new();
    let nested = repo.root.join("nested");
    fs::create_dir(&nested).unwrap();
    git(&nested, &["init", "-q", "-b", "main"]);
    fs::write(nested.join("n"), "n").unwrap();
    // Without a commit git cannot record it at all.
    let e = tree::tree_id(&repo.checkout()).unwrap_err();
    assert_eq!(e.exit, Exit::Refused, "{}", e.message);
    // With a commit git would record it as a gitlink.
    git(&nested, &["add", "n"]);
    git(&nested, &["commit", "-q", "-m", "n"]);
    let e = tree::tree_id(&repo.checkout()).unwrap_err();
    assert!(e.message.contains("nested"), "{}", e.message);
    // Ignored, it is not part of the tree.
    repo.write(".gitignore", "nested/\n");
    assert!(tree::tree_id(&repo.checkout()).is_ok());
}

#[test]
fn temporary_indexes_of_dead_ateliers_are_removed() {
    let repo = Repo::new();
    let checkout = repo.checkout();
    let dead = repo.root.join(".git/atelier-tree-index-9999999-00aa");
    let live = repo.root.join(format!(
        ".git/atelier-tree-index-{}-00bb",
        std::process::id()
    ));
    fs::write(&dead, "x").unwrap();
    fs::write(&live, "x").unwrap();
    tree::tree_id(&checkout).unwrap();
    assert!(!dead.exists(), "a dead atelier's leftover is removed");
    assert!(live.exists(), "a live atelier's index is left alone");
    fs::remove_file(live).unwrap();
}

#[test]
fn git_can_run_in_its_own_process_group() {
    let repo = Repo::new();
    let out = atelier::git::command_own_group(&repo.root)
        .args(["rev-parse", "--git-dir"])
        .output()
        .unwrap();
    assert!(out.status.success());
    // Its group is not this test's: the probe is a shell that prints its own pgid.
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.args(["-c", "ps -o pgid= -p $$"]);
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let pgid: i32 = String::from_utf8_lossy(&cmd.output().unwrap().stdout)
        .trim()
        .parse()
        .unwrap();
    // SAFETY: getpgrp has no preconditions.
    assert_ne!(pgid, unsafe { libc::getpgrp() });
}
