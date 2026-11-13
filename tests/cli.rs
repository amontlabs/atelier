//! The dispatcher, the pin, preflight and exit codes, through the real binary.

mod common;

use std::fs;

use common::*;

#[test]
fn version_and_help_run_outside_a_project() {
    let dir = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(atelier_bin())
        .arg("--version")
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env_remove("ATELIER_DISPATCHED")
        .output()
        .unwrap();
    assert_eq!(code(&out), 0);
    assert_eq!(stdout(&out).trim(), format!("atelier {VERSION}"));

    let out = std::process::Command::new(atelier_bin())
        .arg("--help")
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env_remove("ATELIER_DISPATCHED")
        .output()
        .unwrap();
    assert_eq!(code(&out), 0);
    assert!(stdout(&out).contains("Usage: atelier"));
}

#[test]
fn other_commands_refuse_outside_a_project() {
    let repo = Repo::new();
    for args in [&["check"][..], &["status"], &["rules"], &["merge", "a"]] {
        let out = repo.atelier(args);
        assert_eq!(code(&out), 4, "{args:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("not in an atelier project"),
            "{}",
            stderr(&out)
        );
    }
    // Nothing was created in the repository.
    assert!(!repo.root.join(".git/atelier").exists());
}

#[test]
fn init_outside_a_project_needs_the_version_named() {
    let repo = Repo::new();
    let out = repo.atelier(&["init"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    assert!(stderr(&out).contains("init --version"));

    let out = repo.atelier(&["init", "--version", "../../evil"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
}

#[test]
fn a_missing_version_prints_the_install_command_and_exits_4() {
    let repo = Repo::new();
    let out = repo.atelier(&["init", "--version", "9.9.9"]);
    assert_eq!(code(&out), 4);
    assert!(
        stderr(&out).contains(
            "cargo install --git https://github.com/amontlabs/atelier --tag v9.9.9 --locked --root ~/.local/share/atelier/9.9.9"
        ),
        "{}",
        stderr(&out)
    );

    repo.write_settings("9.9.9");
    for args in [&["check"][..], &["--version"], &["status"]] {
        let out = repo.atelier(args);
        assert_eq!(code(&out), 4, "{args:?}");
        assert!(
            stderr(&out).contains("atelier 9.9.9 is not installed"),
            "{}",
            stderr(&out)
        );
    }
    // Nothing installs itself.
    assert!(!repo.home.join(".local/share/atelier/9.9.9").exists());
}

#[test]
fn init_runs_the_named_version_outside_a_project() {
    let repo = Repo::new();
    repo.install_self(VERSION);
    let out = repo.atelier(&["init", "--version", VERSION]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert!(repo.root.join(".git/atelier/state.lock").is_file());
    assert!(repo.root.join(".atelier/atelier.toml").is_file());

    let out = repo.atelier(&["init"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(stderr(&out).contains(".atelier/ exists"));
}

#[test]
fn the_dispatcher_runs_the_pinned_version() {
    let repo = Repo::new();
    repo.write_settings("9.9.9");
    let bin = repo.home.join(".local/share/atelier/9.9.9/bin");
    fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("atelier");
    fs::write(
        &fake,
        "#!/bin/sh\necho \"dispatched=$ATELIER_DISPATCHED args=$*\"\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let out = repo.atelier(&["check", "--compile-only"]);
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim(),
        "dispatched=1 args=check --compile-only"
    );

    // From a subfolder too: the pin is the checkout's.
    fs::create_dir_all(repo.root.join("sub/dir")).unwrap();
    let out = repo.atelier_in(&repo.root.join("sub/dir"), &["--version"]);
    assert_eq!(stdout(&out).trim(), "dispatched=1 args=--version");
}

#[test]
fn the_program_compares_its_own_version_with_the_pin() {
    let repo = Repo::new();
    repo.write_settings("9.9.9");
    // Called directly and marked dispatched: no dispatch, but the version check still refuses.
    let out = std::process::Command::new(atelier_bin())
        .arg("check")
        .current_dir(&repo.root)
        .env("HOME", &repo.home)
        .env("ATELIER_DISPATCHED", "1")
        .output()
        .unwrap();
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(stderr(&out).contains(&format!("pins atelier 9.9.9; this program is {VERSION}")));
}

#[test]
fn each_worktree_runs_the_version_its_tree_pins() {
    let repo = Repo::new();
    repo.install_self(VERSION);
    repo.write_settings(VERSION);
    repo.commit_all("pin");
    let wt = repo.tmp.path().canonicalize().unwrap().join("wt");
    git(
        &repo.root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "upgrade",
            wt.to_str().unwrap(),
        ],
    );
    fs::write(
        wt.join(".atelier/atelier.toml"),
        "atelier = \"9.9.9\"\n[platforms.macos-aarch64]\nrun = \"check\"\n",
    )
    .unwrap();

    let out = repo.atelier(&["check"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    assert!(stderr(&out).contains("check: not built yet"));

    let out = repo.atelier_in(&wt, &["check"]);
    assert_eq!(code(&out), 4);
    assert!(stderr(&out).contains("atelier 9.9.9 is not installed"));

    // One state folder, in the common git dir, shared by every worktree.
    assert!(repo.root.join(".git/atelier/state.lock").is_file());
    assert!(!repo.root.join(".git/worktrees/wt/atelier").exists());
}

#[test]
fn stub_commands_preflight_then_exit_2() {
    let repo = Repo::new();
    repo.install_self(VERSION);
    repo.write_settings(VERSION);
    repo.commit_all("pin");
    for (args, module) in [
        (&["check"][..], "3.2"),
        (&["start", "a/b"], "3.3"),
        (&["follow"], "3.3"),
        (&["approve", "a", "abc"], "3.4"),
        (&["merge", "a", "b"], "3.4"),
        (&["promote"], "3.4"),
        (&["tmp", "--", "true"], "3.5"),
        (&["clean"], "3.5"),
        (&["test", "--all"], "3.6"),
        (&["machines"], "3.6"),
        (&["status"], "3.8"),
    ] {
        let out = repo.atelier(args);
        assert_eq!(code(&out), 2, "{args:?}: {}", stderr(&out));
        assert!(
            stderr(&out).contains(&format!("not built yet (module {module})")),
            "{args:?}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn usage_errors_exit_2() {
    let repo = Repo::new();
    repo.install_self(VERSION);
    repo.write_settings(VERSION);
    for args in [
        &["merge"][..],
        &["bogus"],
        &["test"],
        &["approve", "a"],
        &["merge", "a", "--waive", "linux-x86_64"],
    ] {
        let out = repo.atelier(args);
        assert_eq!(code(&out), 2, "{args:?}: {}", stderr(&out));
    }
}

#[test]
fn invalid_settings_refuse_with_4() {
    let repo = Repo::new();
    repo.install_self(VERSION);
    for (text, expect) in [
        (
            format!(
                "atelier = \"{VERSION}\"\ncolour = 1\n[platforms.macos-aarch64]\nrun = \"check\"\n"
            ),
            "unknown field `colour`",
        ),
        (
            format!(
                "atelier = \"{VERSION}\"\n[platforms.macos-aarch64]\nrun = \"check\"\nextra = 1\n"
            ),
            "unknown field `extra`",
        ),
        (
            format!(
                "atelier = \"{VERSION}\"\n[[check.steps]]\nname = \"x\"\nrun = [\"scripts/missing.sh\"]\n[platforms.macos-aarch64]\nrun = \"check\"\n"
            ),
            "scripts/missing.sh does not exist",
        ),
        (
            format!(
                "atelier = \"{VERSION}\"\n[[check.steps]]\nname = \"x\"\nrun = [\"../outside.sh\"]\n[platforms.macos-aarch64]\nrun = \"check\"\n"
            ),
            "leaves the checkout",
        ),
        (
            format!(
                "atelier = \"{VERSION}\"\nworkspaces = [\"nope\"]\n[rust]\nnextest = \"0.9.151\"\n[platforms.macos-aarch64]\nrun = \"check\"\n"
            ),
            "nope does not exist",
        ),
        (
            format!("atelier = \"{VERSION}\"\n[platforms.darwin]\nrun = \"check\"\n"),
            "not a platform",
        ),
        (format!("atelier = \"{VERSION}\"\n"), "no [platforms"),
    ] {
        repo.write(".atelier/atelier.toml", &text);
        let out = repo.atelier(&["check"]);
        assert_eq!(code(&out), 4, "{text}: {}", stderr(&out));
        assert!(stderr(&out).contains(expect), "{text}: {}", stderr(&out));
    }
}

#[test]
fn versioned_commands_need_the_version_pinned_on_main() {
    let repo = Repo::new();
    repo.install_self(VERSION);
    repo.write_settings(VERSION);
    // main has no settings yet.
    let out = repo.atelier(&["approve", "a", "abc"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("needs .atelier/atelier.toml on main"),
        "{}",
        stderr(&out)
    );
    assert!(!repo.root.join(".git/atelier/versioned").exists());

    // main pins another version: an upgrade branch's checkout uses the frozen layer only.
    repo.write(
        ".atelier/atelier.toml",
        "atelier = \"9.9.9\"\n[platforms.macos-aarch64]\nrun = \"check\"\n",
    );
    repo.write_rules();
    repo.commit_all("pin 9.9.9 on main");
    git(&repo.root, &["switch", "-q", "-c", "downgrade"]);
    repo.write_settings(VERSION);
    let out = repo.atelier(&["approve", "a", "abc"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(stderr(&out).contains("runs only with the atelier version pinned on main (9.9.9)"));
    assert!(!repo.root.join(".git/atelier/versioned").exists());
    // Commands of the frozen layer still run.
    assert_eq!(code(&repo.atelier(&["check"])), 2);

    // The version pinned on main creates the layer with its format.
    repo.commit_all("pin back");
    git(&repo.root, &["switch", "-q", "main"]);
    git(&repo.root, &["merge", "-q", "--ff-only", "downgrade"]);
    let out = repo.atelier(&["approve", "a", "abc"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    assert_eq!(
        fs::read_to_string(repo.root.join(".git/atelier/versioned/format")).unwrap(),
        "1\n"
    );

    // A newer format is refused, and status reports it without converting.
    fs::write(repo.root.join(".git/atelier/versioned/format"), "7\n").unwrap();
    let out = repo.atelier(&["merge", "a"]);
    assert_eq!(code(&out), 4);
    assert!(
        stderr(&out).contains("state format is newer: atelier follow"),
        "{}",
        stderr(&out)
    );
    let out = repo.atelier(&["status"]);
    assert!(
        stdout(&out).contains("FAILED state format: state format is newer"),
        "{}",
        stdout(&out)
    );
    assert_eq!(
        fs::read_to_string(repo.root.join(".git/atelier/versioned/format")).unwrap(),
        "7\n"
    );
}

#[test]
fn status_reports_every_failed_preflight_line() {
    let repo = Repo::new();
    repo.install_self(VERSION);
    repo.write(
        ".atelier/atelier.toml",
        &format!(
            "atelier = \"{VERSION}\"\nbogus = true\n[platforms.macos-aarch64]\nrun = \"check\"\n"
        ),
    );
    let out = repo.atelier(&["status"]);
    assert_eq!(code(&out), 2);
    let text = stdout(&out);
    assert!(text.contains("ok     version"), "{text}");
    assert!(text.contains("FAILED settings"), "{text}");
}

#[test]
fn a_symbolic_integration_branch_is_refused() {
    let repo = Repo::new();
    repo.install_self(VERSION);
    repo.write(
        ".atelier/atelier.toml",
        &format!("atelier = \"{VERSION}\"\nintegration_branch = \"trunk\"\n[platforms.macos-aarch64]\nrun = \"check\"\n"),
    );
    repo.write_rules();
    repo.commit_all("pin");
    git(
        &repo.root,
        &["symbolic-ref", "refs/heads/trunk", "refs/heads/main"],
    );
    let out = repo.atelier(&["approve", "a", "abc"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("refs/heads/trunk is a symbolic ref"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn versioned_commands_run_only_in_the_main_checkout_on_its_integration_branch() {
    let repo = Repo::new();
    repo.install_self(VERSION);
    let settings = |branch: &str| {
        format!(
            "atelier = \"{VERSION}\"\nintegration_branch = \"{branch}\"\n[platforms.macos-aarch64]\nrun = \"check\"\n"
        )
    };
    // main pins another version.
    repo.write(
        ".atelier/atelier.toml",
        "atelier = \"9.9.9\"\n[platforms.macos-aarch64]\nrun = \"check\"\n",
    );
    repo.commit_all("pin 9.9.9 on main");

    // The review's case: an upgrade branch pins this version and names itself the integration
    // branch, in its own worktree. The worktree's settings play no part.
    let wt = repo.tmp.path().join("up");
    git(
        &repo.root,
        &["worktree", "add", "-q", "-b", "up", wt.to_str().unwrap()],
    );
    fs::write(wt.join(".atelier/atelier.toml"), settings("up")).unwrap();
    git(&wt, &["add", "-A"]);
    git(&wt, &["commit", "-q", "-m", "up names itself"]);
    let out = repo.atelier_in(&wt, &["approve", "a", "abc"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("runs only with the atelier version pinned on main (9.9.9)"),
        "{}",
        stderr(&out)
    );
    assert!(!repo.root.join(".git/atelier/versioned").exists());

    // With main pinning this version, a linked worktree is still refused.
    repo.write(".atelier/atelier.toml", &settings("main"));
    repo.write_rules();
    repo.commit_all("pin this version on main");
    let out = repo.atelier_in(&wt, &["approve", "a", "abc"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("runs only in the main checkout"),
        "{}",
        stderr(&out)
    );
    assert!(!repo.root.join(".git/atelier/versioned").exists());

    // The main checkout on another branch that names main is refused too.
    git(&repo.root, &["switch", "-q", "-c", "feature"]);
    let out = repo.atelier(&["approve", "a", "abc"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("runs only in the main checkout on main; it is on feature"),
        "{}",
        stderr(&out)
    );
    // A detached HEAD as well.
    git(&repo.root, &["switch", "-q", "--detach"]);
    let out = repo.atelier(&["approve", "a", "abc"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(stderr(&out).contains("detached"), "{}", stderr(&out));
    assert!(!repo.root.join(".git/atelier/versioned").exists());

    // On main, in the main checkout: the preflight passes (the command is a stub, exit 2).
    git(&repo.root, &["switch", "-q", "main"]);
    let out = repo.atelier(&["approve", "a", "abc"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    assert!(repo.root.join(".git/atelier/versioned/format").is_file());
}
