//! Helpers for tests on throwaway git repositories, each in its own temp folder.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The atelier binary under test.
pub fn atelier_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_atelier"))
}

/// A git command isolated from the user's and the system's configuration.
pub fn git_cmd(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args([
            "-c",
            "user.name=atelier-test",
            "-c",
            "user.email=test@atelier.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.file.allow=always",
        ]);
    c
}

/// Runs git, requires success, returns trimmed stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_cmd(dir).args(args).output().expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A throwaway repository on `main` with one commit, inside its own temp folder, plus a temp
/// HOME for the program.
pub struct Repo {
    pub tmp: TempDir,
    pub root: PathBuf,
    pub home: PathBuf,
}

impl Repo {
    pub fn new() -> Repo {
        let tmp = tempfile::tempdir().expect("temp folder");
        // Canonical: macOS temp folders sit behind the /var -> /private/var link.
        let base = fs::canonicalize(tmp.path()).unwrap();
        let root = base.join("repo");
        let home = base.join("home");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&home).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("README"), "hello\n").unwrap();
        git(&root, &["add", "README"]);
        git(&root, &["commit", "-q", "-m", "first"]);
        Repo { tmp, root, home }
    }

    pub fn write(&self, rel: &str, contents: &str) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, contents).unwrap();
    }

    pub fn commit_all(&self, msg: &str) -> String {
        git(&self.root, &["add", "-A"]);
        git(&self.root, &["commit", "-q", "-m", msg]);
        git(&self.root, &["rev-parse", "HEAD"])
    }

    pub fn checkout(&self) -> atelier::git::Checkout {
        atelier::git::Checkout::find(&self.root).unwrap().unwrap()
    }

    /// Minimal valid settings pinned to `pin`.
    pub fn write_settings(&self, pin: &str) {
        self.write(
            ".atelier/atelier.toml",
            &format!("atelier = \"{pin}\"\n\n[platforms.macos-aarch64]\nrun = \"check\"\n"),
        );
        self.write_rules();
    }

    /// Writes the generated rules for the settings in the checkout.
    pub fn write_rules(&self) {
        let settings = atelier::settings::Settings::load(&self.root).unwrap();
        atelier::rules::write(&self.checkout(), &settings).unwrap();
    }

    /// The owner's agents file in this HOME.
    pub fn write_agents_file(&self, text: &str) {
        let p = self.home.join(".config/atelier/agents.toml");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    /// Makes the binary under test the installed copy of `version` in this HOME.
    pub fn install_self(&self, version: &str) -> PathBuf {
        let bin = self
            .home
            .join(format!(".local/share/atelier/{version}/bin"));
        fs::create_dir_all(&bin).unwrap();
        let target = bin.join("atelier");
        std::os::unix::fs::symlink(atelier_bin(), &target).unwrap();
        target
    }

    /// The program, started like the dispatcher on PATH, in `dir`, with this HOME.
    pub fn atelier_in(&self, dir: &Path, args: &[&str]) -> Output {
        self.atelier_env(dir, args, &[])
    }

    /// The program in `dir` with extra environment variables (`PATH`, for one). Commits it
    /// makes (a rebase) get the test identity.
    pub fn atelier_env(&self, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = Command::new(atelier_bin());
        c.args(args)
            .current_dir(dir)
            .env("HOME", &self.home)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "atelier-test")
            .env("GIT_AUTHOR_EMAIL", "test@atelier.invalid")
            .env("GIT_COMMITTER_NAME", "atelier-test")
            .env("GIT_COMMITTER_EMAIL", "test@atelier.invalid")
            .env_remove("ATELIER_DISPATCHED")
            .env_remove("ATELIER_RUN");
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().expect("atelier runs")
    }

    pub fn atelier(&self, args: &[&str]) -> Output {
        self.atelier_in(&self.root, args)
    }
}

pub fn code(out: &Output) -> i32 {
    out.status.code().expect("exited normally")
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}
