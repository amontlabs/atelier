//! `atelier check`, through the real binary, on throwaway repositories holding a tiny Cargo
//! workspace. Plain cargo does the building: a `cargo` link in a folder first on PATH keeps the
//! machine's cargo wrapper (and mbx) out. Tests that need the mbx prerequisite fake `mbx` with a
//! script.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use atelier::locks::{HeldLock, LockKey};
use atelier::runs::{RunInfo, RunKind, RunResult, TestCounts};
use atelier::state::State;
use common::*;

const GOOD_LIB: &str = "pub fn add(a: u32, b: u32) -> u32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(1, 2), 3);
    }

    #[test]
    fn adds_zero() {
        assert_eq!(add(0, 0), 0);
    }
}
";

struct Fx {
    repo: Repo,
    /// The folder first on PATH, holding a plain `cargo`.
    plain: PathBuf,
    nextest: String,
}

fn real_home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME"))
}

fn toolchain() -> String {
    let text =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("rust-toolchain.toml"))
            .unwrap();
    let table: toml::Table = toml::from_str(&text).unwrap();
    table["toolchain"]["channel"].as_str().unwrap().to_owned()
}

impl Fx {
    fn new(extra_settings: &str) -> Fx {
        let repo = Repo::new();
        let plain = repo.tmp.path().join("plainbin");
        fs::create_dir_all(&plain).unwrap();
        let proxy = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| real_home().join(".cargo"))
            .join("bin/cargo");
        if proxy.is_file() {
            std::os::unix::fs::symlink(&proxy, plain.join("cargo")).unwrap();
        }
        let mut fx = Fx {
            repo,
            plain,
            nextest: String::new(),
        };
        let v = fx.run_cmd(
            Command::new("cargo").args(["nextest", "--version"]),
            &fx.repo.root,
        );
        fx.nextest = stdout(&v)
            .split_whitespace()
            .nth(1)
            .expect("cargo-nextest is installed")
            .to_owned();

        let r = &fx.repo;
        r.write(".gitignore", "target\nignored.txt\n.mbx.toml\n");
        r.write(
            "rust-toolchain.toml",
            &format!("[toolchain]\nchannel = \"{}\"\n", toolchain()),
        );
        r.write(
            "Cargo.toml",
            "[workspace]\nmembers = [\"a\"]\nresolver = \"3\"\n",
        );
        r.write(
            "a/Cargo.toml",
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        r.write("a/src/lib.rs", GOOD_LIB);
        fx.write_settings(extra_settings);
        let out = fx.run_cmd(
            Command::new("cargo").arg("generate-lockfile"),
            &fx.repo.root,
        );
        assert_eq!(code(&out), 0, "{}", stderr(&out));
        fx.repo.commit_all("workspace");
        fx.repo.install_self(VERSION);
        fx
    }

    fn write_settings(&self, extra: &str) {
        self.repo.write(
            ".atelier/atelier.toml",
            &format!(
                "atelier = \"{VERSION}\"\nworkspaces = [\".\"]\n\n[rust]\nnextest = \"{}\"\n\n\
                 [platforms.macos-aarch64]\nrun = \"check\"\n[platforms.linux-x86_64]\nrun = \"check\"\n\
                 [platforms.linux-aarch64]\nrun = \"check\"\n[platforms.macos-x86_64]\nrun = \"check\"\n\n{extra}",
                self.nextest
            ),
        );
        self.repo.write_rules();
    }

    /// Settings pinned to the real nextest, after the fixture has been built.
    fn set_settings(&self, extra: &str) {
        self.write_settings(extra);
        self.repo.commit_all("settings");
    }

    fn path(&self) -> std::ffi::OsString {
        let mut dirs = vec![self.plain.clone()];
        if let Some(p) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&p));
        }
        std::env::join_paths(dirs).unwrap()
    }

    fn run_cmd(&self, cmd: &mut Command, dir: &Path) -> Output {
        self.run_cmd_env(cmd, dir, &[])
    }

    fn run_cmd_env(&self, cmd: &mut Command, dir: &Path, env: &[(&str, &str)]) -> Output {
        cmd.current_dir(dir)
            .env("PATH", self.path())
            .env("HOME", &self.repo.home)
            .env(
                "RUSTUP_HOME",
                std::env::var_os("RUSTUP_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| real_home().join(".rustup")),
            )
            .env(
                "CARGO_HOME",
                std::env::var_os("CARGO_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| real_home().join(".cargo")),
            )
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("ATELIER_DISPATCHED")
            .env_remove("ATELIER_RUN")
            .env_remove("ATELIER_RUNS")
            .env_remove("MBX_DISABLE")
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_TARGET_DIR")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("CARGO_BUILD_RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .env_remove("NEXTEST_RETRIES")
            .env_remove("NEXTEST_FLAKY_RESULT");
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().expect("runs")
    }

    fn check(&self, args: &[&str]) -> Output {
        self.check_env(args, &[])
    }

    fn check_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(atelier_bin());
        cmd.arg("check").args(args);
        self.run_cmd_env(&mut cmd, &self.repo.root, env)
    }

    /// Every run folder of this clone.
    fn runs(&self) -> Vec<PathBuf> {
        let dir = self.repo.root.join(".git/atelier/runs");
        let mut v: Vec<PathBuf> = fs::read_dir(dir)
            .map(|d| d.map(|e| e.unwrap().path()).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    fn last_run(&self) -> PathBuf {
        self.runs().pop().expect("a run folder")
    }

    /// Installs a fake `mbx` and returns the storage folder it reports.
    fn fake_mbx(&self, version: &str) -> PathBuf {
        let storage = self.repo.tmp.path().join("mbx-storage");
        fs::create_dir_all(&storage).unwrap();
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'mbx {version}';;\n  settings) case \"$3\" in\n    build_script_execution) echo false;;\n    target.root) echo '{}';;\n    *) echo;;\n  esac;;\nesac\n",
            storage.display()
        );
        let path = self.plain.join("mbx");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        storage
    }

    fn mbx_settings(&self, extra: &str) -> String {
        format!(
            "[prerequisites.mbx]\nversion = \"1.30.0\"\ncargo = [\"{}\"]\nsettings = {{ build_script_execution = false }}\n{extra}",
            self.plain.join("cargo").display()
        )
    }
}

fn last_line(out: &Output) -> String {
    stdout(out)
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_owned()
}

fn result_text(run: &Path) -> String {
    fs::read_to_string(run.join("result")).unwrap()
}

fn shell_step(name: &str, script: &str) -> String {
    format!("[[check.steps]]\nname = \"{name}\"\nrun = [\"sh\", \"-c\", {script:?}]\n")
}

#[test]
fn a_green_check_prints_the_design_line_and_records_every_test() {
    let fx = Fx::new("");
    let out = fx.check(&[]);
    assert_eq!(code(&out), 0, "{}\n{}", stdout(&out), stderr(&out));
    let line = last_line(&out);
    let run = fx.last_run();
    let id = run.file_name().unwrap().to_str().unwrap();
    let head = git(&fx.repo.root, &["rev-parse", "--short=7", "HEAD"]);
    assert!(
        line.starts_with(&format!(
            "check: GREEN — run {id}, commit {head} (clean), 2 tests, 0 excluded, "
        )),
        "{line}"
    );
    assert!(
        line.ends_with(&format!(", log {}", run.display())),
        "{line}"
    );

    assert!(result_text(&run).contains("outcome = \"passed\""));
    let result = RunResult::read(&run).unwrap().unwrap();
    assert_eq!(result.kind, RunKind::Check);
    assert_eq!(
        result.tests,
        Some(TestCounts {
            passed: 2,
            ..TestCounts::default()
        })
    );
    assert_eq!(result.test_results.as_deref(), Some("tests.tsv"));
    let info = RunInfo::read(&run).unwrap().unwrap();
    assert_eq!(info.kind, RunKind::Check);
    assert_eq!(info.runs, vec![id.to_owned()]);
    let tsv = fs::read_to_string(run.join("tests.tsv")).unwrap();
    assert_eq!(tsv.lines().count(), 2, "{tsv}");
    assert!(tsv.contains("\tpass\t"), "{tsv}");
    assert!(run.join("tests-root.jsonl").is_file());
    for log in ["fmt.log", "compile.log", "tests.log"] {
        assert!(run.join(log).is_file(), "{log}");
    }
}

#[test]
fn uncommitted_changes_are_noted_in_the_line() {
    let fx = Fx::new("");
    fx.repo.write("notes.txt", "scratch\n");
    let out = fx.check(&[]);
    assert_eq!(code(&out), 0, "{}", stdout(&out));
    assert!(
        last_line(&out).contains("(+ uncommitted changes)"),
        "{}",
        last_line(&out)
    );
}

#[test]
fn a_format_failure_is_red_and_later_steps_still_run() {
    let fx = Fx::new(&shell_step("links", "exit 0"));
    fx.repo
        .write("a/src/lib.rs", &GOOD_LIB.replace("a + b", "a  +  b"));
    fx.repo.commit_all("badly formatted");
    let out = fx.check(&[]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
    let line = last_line(&out);
    assert!(line.starts_with("check: RED — run "), "{line}");
    assert!(line.contains("failed: fmt;"), "{line}");
    let text = stdout(&out);
    assert!(text.contains("compile: ok"), "{text}");
    assert!(text.contains("tests: ok"), "{text}");
    assert!(text.contains("links: ok"), "{text}");
}

#[test]
fn a_failing_test_is_red_and_named() {
    let fx = Fx::new("");
    fx.repo.write(
        "a/src/lib.rs",
        &GOOD_LIB.replace("assert_eq!(add(0, 0), 0)", "assert_eq!(add(0, 0), 1)"),
    );
    fx.repo.commit_all("a failing test");
    let out = fx.check(&[]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
    assert!(
        last_line(&out).contains("failed: tests;"),
        "{}",
        last_line(&out)
    );
    assert!(
        stdout(&out).contains("failed test: a::tests::adds_zero")
            || stdout(&out).contains("adds_zero")
    );
    let tsv = fs::read_to_string(fx.last_run().join("tests.tsv")).unwrap();
    assert!(tsv.contains("\tfail\t"), "{tsv}");
    let tests = RunResult::read(&fx.last_run()).unwrap().unwrap().tests;
    assert_eq!(
        tests,
        Some(TestCounts {
            passed: 1,
            failed: 1,
            ..TestCounts::default()
        })
    );
}

#[test]
fn a_compile_failure_skips_the_tests() {
    let fx = Fx::new(&shell_step("links", "exit 0"));
    fx.repo
        .write("a/src/lib.rs", &GOOD_LIB.replace("a + b", "a + \"b\""));
    fx.repo.commit_all("does not compile");
    let out = fx.check(&[]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
    let line = last_line(&out);
    assert!(line.contains("failed: compile;"), "{line}");
    assert!(!line.contains("tests"), "{line}");
    let text = stdout(&out);
    assert!(text.contains("tests: skipped (compile failed)"), "{text}");
    assert!(text.contains("links: ok"), "{text}");
    assert!(!fx.last_run().join("tests.log").exists());
}

#[test]
fn every_project_step_runs_and_each_failure_is_listed() {
    let steps = format!(
        "{}{}{}",
        shell_step("frontend", "exit 3"),
        shell_step("typecheck", "echo fine"),
        shell_step("links", "echo broken >&2; exit 1")
    );
    let fx = Fx::new(&steps);
    let out = fx.check(&[]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
    assert!(
        last_line(&out).contains("failed: frontend, links;"),
        "{}",
        last_line(&out)
    );
    let run = fx.last_run();
    assert!(
        fs::read_to_string(run.join("links.log"))
            .unwrap()
            .contains("broken")
    );
    assert!(
        fs::read_to_string(run.join("typecheck.log"))
            .unwrap()
            .contains("fine")
    );
}

#[test]
fn a_hook_that_edits_a_tracked_file_is_red_naming_it_and_stops_the_run() {
    let hooks = "[[check.prepare]]\nname = \"touch-readme\"\nrun = [\"sh\", \"-c\", \"echo more >> README\"]\n";
    let fx = Fx::new(&format!("{hooks}{}", shell_step("links", "exit 0")));
    let out = fx.check(&[]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
    let text = stdout(&out);
    assert!(text.contains("touch-readme"), "{text}");
    assert!(text.contains("README"), "{text}");
    let line = last_line(&out);
    assert!(line.contains("failed: prepare:touch-readme;"), "{line}");
    let run = fx.last_run();
    assert!(!run.join("fmt.log").exists());
    assert!(!run.join("links.log").exists());
}

#[test]
fn a_failing_hook_stops_the_run() {
    let hooks = "[[check.prepare]]\nname = \"broken\"\nrun = [\"sh\", \"-c\", \"exit 2\"]\n";
    let fx = Fx::new(hooks);
    let out = fx.check(&[]);
    assert_eq!(code(&out), 1);
    assert!(last_line(&out).contains("failed: prepare:broken;"));
    assert!(!fx.last_run().join("fmt.log").exists());
}

#[test]
fn hooks_run_in_order_may_write_ignored_files_and_respect_platforms() {
    let hooks = concat!(
        "[[check.prepare]]\nname = \"one\"\nrun = [\"sh\", \"-c\", \"echo one > ignored.txt\"]\n",
        "[[check.prepare]]\nname = \"two\"\nrun = [\"sh\", \"-c\", \"test -f ignored.txt && echo two >> ignored.txt\"]\n",
        "[[check.prepare]]\nname = \"elsewhere\"\nplatforms = [\"windows-x86_64\"]\nrun = [\"sh\", \"-c\", \"exit 9\"]\n",
    );
    let fx = Fx::new(&format!(
        "{hooks}\n[platforms.windows-x86_64]\nrun = \"check\"\n"
    ));
    let out = fx.check(&[]);
    assert_eq!(code(&out), 0, "{}\n{}", stdout(&out), stderr(&out));
    assert_eq!(
        fs::read_to_string(fx.repo.root.join("ignored.txt")).unwrap(),
        "one\ntwo\n"
    );
    assert!(!stdout(&out).contains("elsewhere"));
}

#[test]
fn a_project_step_that_changes_the_tree_makes_the_run_red() {
    let fx = Fx::new(&shell_step("sneaky", "echo x >> README"));
    let out = fx.check(&[]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
    assert!(
        stdout(&out).contains("the tree changed during the run (README)"),
        "{}",
        stdout(&out)
    );
    assert!(
        last_line(&out).contains("failed: tree;"),
        "{}",
        last_line(&out)
    );
}

#[test]
fn nextest_files_with_retries_a_default_filter_or_flaky_pass_are_refused() {
    for (file, needle) in [
        ("[profile.default]\nretries = 2\n", "retries is 2"),
        (
            "[profile.ci]\nretries = { backoff = \"fixed\", count = 1 }\n",
            "profile.ci.retries is 1",
        ),
        (
            "[profile.default]\ndefault-filter = \"not test(slow)\"\n",
            "default-filter",
        ),
        (
            "[profile.default]\nflaky-result = \"pass\"\n",
            "flaky-result",
        ),
    ] {
        let fx = Fx::new("");
        fx.repo.write(".config/nextest.toml", file);
        fx.repo.commit_all("nextest file");
        let out = fx.check(&[]);
        assert_eq!(code(&out), 4, "{file}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains(".config/nextest.toml"), "{err}");
        assert!(err.contains(needle), "{needle}: {err}");
        assert!(fx.runs().is_empty(), "nothing ran");
    }
}

#[test]
fn a_clean_nextest_file_is_accepted() {
    let fx = Fx::new("");
    fx.repo.write(
        ".config/nextest.toml",
        "[profile.default]\nfail-fast = false\nretries = 0\nflaky-result = \"fail\"\n",
    );
    fx.repo.commit_all("nextest file");
    assert_eq!(code(&fx.check(&[])), 0);
}

#[test]
fn an_exclusion_is_printed_with_its_reason_and_counted() {
    let mut lib = GOOD_LIB.to_owned();
    lib.push_str("\n#[cfg(test)]\nmod slow {\n    #[test]\n    fn slow_engine() {\n        panic!(\"needs the engine\");\n    }\n}\n");
    let fx = Fx::new(
        "[[rust.exclude]]\nfilter = \"test(slow_engine)\"\nreason = \"needs a real engine\"\n",
    );
    fx.repo.write("a/src/lib.rs", &lib);
    fx.repo.commit_all("a slow test");
    let out = fx.check(&[]);
    assert_eq!(code(&out), 0, "{}\n{}", stdout(&out), stderr(&out));
    assert!(
        stdout(&out).contains("excluded: test(slow_engine) (needs a real engine)"),
        "{}",
        stdout(&out)
    );
    assert!(
        last_line(&out).contains(", 2 tests, 1 excluded, "),
        "{}",
        last_line(&out)
    );
    let tests = RunResult::read(&fx.last_run()).unwrap().unwrap().tests;
    assert_eq!(
        tests,
        Some(TestCounts {
            passed: 2,
            excluded: 1,
            ..TestCounts::default()
        })
    );
}

#[test]
fn every_job_runs_without_retries_whatever_the_callers_shell_exports() {
    let fx = Fx::new(&shell_step(
        "env",
        "test \"$NEXTEST_RETRIES\" = 0 && test \"$NEXTEST_FLAKY_RESULT\" = fail",
    ));
    let out = fx.check_env(
        &[],
        &[("NEXTEST_RETRIES", "5"), ("NEXTEST_FLAKY_RESULT", "pass")],
    );
    assert_eq!(code(&out), 0, "{}", stdout(&out));
}

#[test]
fn a_check_inside_another_run_carries_both_run_ids() {
    let outer = "20261113T1012Z-4f2a09c1d3e5b7a8";
    let fx = Fx::new(&shell_step(
        "markers",
        "echo \"$ATELIER_RUN $ATELIER_RUNS\"",
    ));
    let out = fx.check_env(&[], &[("ATELIER_RUN", outer), ("ATELIER_RUNS", outer)]);
    assert_eq!(code(&out), 0, "{}\n{}", stdout(&out), stderr(&out));
    let run = fx.last_run();
    let id = run.file_name().unwrap().to_str().unwrap();
    assert_eq!(
        RunInfo::read(&run).unwrap().unwrap().runs,
        vec![outer.to_owned(), id.to_owned()]
    );
    let log = fs::read_to_string(run.join("markers.log")).unwrap();
    assert_eq!(log.trim(), format!("{id} {outer}:{id}"));
}

#[test]
fn compile_only_runs_the_compile_steps_and_never_says_green() {
    let steps = format!(
        "{}{}",
        shell_step("frontend", "exit 1"),
        "[[check.steps]]\nname = \"typecheck\"\nrun = [\"sh\", \"-c\", \"echo checked\"]\ncompile = true\n"
    );
    let fx = Fx::new(&steps);
    // A failing test does not matter to the compile check.
    fx.repo.write(
        "a/src/lib.rs",
        &GOOD_LIB.replace("assert_eq!(add(0, 0), 0)", "assert_eq!(add(0, 0), 1)"),
    );
    fx.repo.commit_all("a failing test");
    let out = fx.check(&["--compile-only"]);
    assert_eq!(code(&out), 0, "{}\n{}", stdout(&out), stderr(&out));
    let line = last_line(&out);
    assert!(line.starts_with("compile: OK — run "), "{line}");
    assert!(!stdout(&out).contains("GREEN"));
    let run = fx.last_run();
    let result = RunResult::read(&run).unwrap().unwrap();
    assert_eq!(result.kind, RunKind::CheckCompile);
    assert_eq!(result.tests, None);
    assert_eq!(result.test_results, None);
    assert!(run.join("typecheck.log").is_file());
    assert!(!run.join("frontend.log").exists());
    assert!(!run.join("tests.log").exists());

    fx.repo
        .write("a/src/lib.rs", &GOOD_LIB.replace("a + b", "a + \"b\""));
    fx.repo.commit_all("does not compile");
    let out = fx.check(&["--compile-only"]);
    assert_eq!(code(&out), 1);
    assert!(
        last_line(&out).starts_with("compile: FAILED — run "),
        "{}",
        last_line(&out)
    );
}

#[test]
fn a_second_check_in_the_same_worktree_is_refused() {
    let fx = Fx::new("");
    let checkout = fx.repo.checkout();
    let state = State::open(&checkout).unwrap();
    let _held = HeldLock::acquire(&state, LockKey::worktree(&checkout)).unwrap();
    let out = fx.check(&[]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("held by another atelier command"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn prerequisites_are_checked_before_anything_runs() {
    // The wrong nextest pin.
    let fx = Fx::new("");
    let text = fs::read_to_string(fx.repo.root.join(".atelier/atelier.toml")).unwrap();
    fx.repo.write(
        ".atelier/atelier.toml",
        &text.replace(
            &format!("nextest = \"{}\"", fx.nextest),
            "nextest = \"0.9.1\"",
        ),
    );
    fx.repo.commit_all("wrong pin");
    let out = fx.check(&[]);
    assert_eq!(code(&out), 4);
    assert!(
        stderr(&out).contains("the settings pin 0.9.1"),
        "{}",
        stderr(&out)
    );

    // A declared tool that prints something else; one for another platform is not asked.
    let tools = concat!(
        "[[prerequisites.tools]]\nname = \"node\"\nrun = [\"sh\", \"-c\", \"echo v26.1\"]\nexpect = \"v24.\"\n",
        "[[prerequisites.tools]]\nname = \"other\"\nplatforms = [\"windows-x86_64\"]\nrun = [\"sh\", \"-c\", \"exit 1\"]\nexpect = \"x\"\n",
        "[platforms.windows-x86_64]\nrun = \"check\"\n",
    );
    let fx = Fx::new(tools);
    let out = fx.check(&[]);
    assert_eq!(code(&out), 4);
    let err = stderr(&out);
    assert!(
        err.contains("tool node") && err.contains("\"v26.1\""),
        "{err}"
    );
    assert!(!err.contains("tool other"), "{err}");
    assert!(fx.runs().is_empty());

    // A toolchain that the workspace does not pin exactly.
    let fx = Fx::new("");
    fx.repo
        .write("rust-toolchain.toml", "[toolchain]\nchannel = \"stable\"\n");
    fx.repo.commit_all("floating toolchain");
    let out = fx.check(&[]);
    assert_eq!(code(&out), 4);
    assert!(
        stderr(&out).contains("not an exact version"),
        "{}",
        stderr(&out)
    );
}

// ----- mbx -----

#[test]
fn with_mbx_the_listed_setup_is_green_and_every_deviation_is_refused() {
    let fx = Fx::new("");
    let storage = fx.fake_mbx("1.30.0");
    fx.set_settings(&fx.mbx_settings(""));
    std::os::unix::fs::symlink(storage.join("t1"), fx.repo.root.join("target")).unwrap();
    fs::create_dir_all(storage.join("t1")).unwrap();

    let out = fx.check(&[]);
    assert_eq!(code(&out), 0, "{}\n{}", stdout(&out), stderr(&out));

    // A `.mbx.toml` that contradicts the settings: 4, naming file and key. The same value is fine.
    fx.repo
        .write(".mbx.toml", "build_script_execution = false\n");
    assert_eq!(code(&fx.check(&[])), 0);
    fx.repo
        .write(".mbx.toml", "build_script_execution = true\n");
    let out = fx.check(&[]);
    assert_eq!(code(&out), 4, "{}", stdout(&out));
    let err = stderr(&out);
    assert!(
        err.contains(".mbx.toml sets build_script_execution = true"),
        "{err}"
    );
    fs::remove_file(fx.repo.root.join(".mbx.toml")).unwrap();

    // Bypass variables.
    for var in [
        "MBX_DISABLE",
        "CARGO_TARGET_DIR",
        "RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
    ] {
        let out = fx.check_env(&[], &[(var, "1")]);
        assert_eq!(code(&out), 4, "{var}");
        assert!(
            stderr(&out).contains(&format!("{var} is set")),
            "{}",
            stderr(&out)
        );
    }

    // The mbx version.
    fx.fake_mbx("1.29.0");
    let out = fx.check(&[]);
    assert_eq!(code(&out), 4);
    assert!(
        stderr(&out).contains("the settings pin 1.30.0"),
        "{}",
        stderr(&out)
    );
    fx.fake_mbx("1.30.0");

    // A build folder plain cargo made.
    fs::remove_file(fx.repo.root.join("target")).unwrap();
    fs::create_dir(fx.repo.root.join("target")).unwrap();
    let out = fx.check(&[]);
    assert_eq!(code(&out), 4);
    assert!(
        stderr(&out).contains("real folder that plain cargo made"),
        "{}",
        stderr(&out)
    );
    fs::remove_dir(fx.repo.root.join("target")).unwrap();

    // A link out of mbx's storage.
    let elsewhere = fx.repo.tmp.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, fx.repo.root.join("target")).unwrap();
    let out = fx.check(&[]);
    assert_eq!(code(&out), 4);
    assert!(
        stderr(&out).contains("not in mbx's storage"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn the_first_cargo_on_path_must_be_a_listed_file() {
    let fx = Fx::new("");
    fx.fake_mbx("1.30.0");
    fx.set_settings(
        "[prerequisites.mbx]\nversion = \"1.30.0\"\ncargo = [\"/nonexistent/cargo\"]\n",
    );
    let out = fx.check(&[]);
    assert_eq!(code(&out), 4);
    assert!(
        stderr(&out).contains("the first `cargo` on PATH is"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn an_mbx_file_that_appears_during_the_run_is_red() {
    let fx = Fx::new("");
    let storage = fx.fake_mbx("1.30.0");
    fx.set_settings(&format!(
        "{}{}",
        fx.mbx_settings(""),
        shell_step("late", "echo 'linker = \"lld\"' > .mbx.toml")
    ));
    fs::create_dir_all(storage.join("t1")).unwrap();
    std::os::unix::fs::symlink(storage.join("t1"), fx.repo.root.join("target")).unwrap();
    let out = fx.check(&[]);
    assert_eq!(code(&out), 1, "{}\n{}", stdout(&out), stderr(&out));
    assert!(
        stdout(&out).contains(".mbx.toml appeared during the run"),
        "{}",
        stdout(&out)
    );
    assert!(
        last_line(&out).contains("failed: mbx;"),
        "{}",
        last_line(&out)
    );
}

#[test]
fn a_build_folder_made_by_plain_cargo_during_the_run_is_red() {
    let fx = Fx::new("");
    fx.fake_mbx("1.30.0");
    fx.set_settings(&fx.mbx_settings(""));
    // No `target` before: the build creates a real folder, which the second look rejects.
    let out = fx.check(&[]);
    assert_eq!(code(&out), 1, "{}\n{}", stdout(&out), stderr(&out));
    assert!(
        stdout(&out).contains("real folder that plain cargo made"),
        "{}",
        stdout(&out)
    );
}

// ----- the review's probes (module 3.2 review, findings 1-11) -----

/// A nextest file the project ignores in git, so a hook may write it without changing the tree.
fn ignore(fx: &Fx, lines: &str) {
    let text = fs::read_to_string(fx.repo.root.join(".gitignore")).unwrap();
    fx.repo.write(".gitignore", &format!("{text}{lines}"));
    fx.repo.commit_all("ignore more");
}

#[test]
fn nextest_files_that_let_a_failing_test_pass_or_vanish_are_refused() {
    for (file, needle) in [
        // Finding 1: a hung test counts as passed.
        (
            "[profile.default]\nslow-timeout = { period = \"1s\", terminate-after = 2, on-timeout = \"pass\" }\n",
            "profile.default.slow-timeout sets on-timeout",
        ),
        (
            "[[profile.default.overrides]]\nfilter = 'all()'\nslow-timeout = { period = \"1s\", on-timeout = \"pass\" }\n",
            "overrides[0].slow-timeout sets on-timeout",
        ),
        // Finding 2: a wrapper swallows every failure.
        (
            "experimental = [\"wrapper-scripts\"]\n[scripts.wrapper.swallow]\ncommand = 'sh'\n\
             [[profile.default.scripts]]\nfilter = 'all()'\nrun-wrapper = 'swallow'\n",
            "scripts[0].run-wrapper",
        ),
        (
            "experimental = [\"wrapper-scripts\"]\n",
            "experimental wrapper-scripts",
        ),
        // Finding 3: a default filter inside an override.
        (
            "[[profile.default.overrides]]\nplatform = 'cfg(all())'\ndefault-filter = 'not test(only_ci)'\n",
            "overrides[0].default-filter",
        ),
    ] {
        let fx = Fx::new("");
        fx.repo.write(".config/nextest.toml", file);
        fx.repo.commit_all("nextest file");
        let out = fx.check(&[]);
        assert_eq!(code(&out), 4, "{file}: {}\n{}", stdout(&out), stderr(&out));
        let err = stderr(&out);
        assert!(err.contains(needle), "{needle}: {err}");
        assert!(fx.runs().is_empty(), "nothing ran");
    }
}

#[test]
fn nextest_setup_scripts_are_accepted() {
    let fx = Fx::new("");
    fx.repo.write(
        ".config/nextest.toml",
        "experimental = [\"setup-scripts\"]\n[scripts.setup.s]\ncommand = 'true'\n\
         [[profile.default.scripts]]\nfilter = 'all()'\nsetup = 's'\n",
    );
    fx.repo.commit_all("setup script");
    assert_eq!(code(&fx.check(&[])), 0);
}

#[test]
fn a_hook_edit_of_an_assume_unchanged_file_changes_the_tree() {
    // Finding 4.
    let hook = "[[check.prepare]]\nname = \"add-test\"\nrun = [\"sh\", \"-c\", \"printf '\\\\n#[cfg(test)]\\\\nmod hook {\\\\n    #[test]\\\\n    fn hook_added() {}\\\\n}\\\\n' >> a/src/lib.rs\"]\n";
    let fx = Fx::new(hook);
    git(
        &fx.repo.root,
        &["update-index", "--assume-unchanged", "a/src/lib.rs"],
    );
    let out = fx.check(&[]);
    assert_eq!(code(&out), 1, "{}\n{}", stdout(&out), stderr(&out));
    assert!(
        stdout(&out).contains("hook add-test changed the tree (a/src/lib.rs)"),
        "{}",
        stdout(&out)
    );
    assert!(
        last_line(&out).contains("failed: prepare:add-test;"),
        "{}",
        last_line(&out)
    );
}

#[test]
fn a_tree_with_edits_under_assume_unchanged_or_skip_worktree_is_not_called_clean() {
    let fx = Fx::new("");
    git(
        &fx.repo.root,
        &["update-index", "--assume-unchanged", "a/src/lib.rs"],
    );
    git(
        &fx.repo.root,
        &["update-index", "--skip-worktree", "README"],
    );
    fx.repo.write("README", "edited\n");
    let out = fx.check(&[]);
    assert_eq!(code(&out), 0, "{}\n{}", stdout(&out), stderr(&out));
    assert!(
        last_line(&out).contains("(+ uncommitted changes)"),
        "{}",
        last_line(&out)
    );

    // And a project step that edits a skip-worktree file is caught at the end.
    let fx = Fx::new(&shell_step("sneaky", "echo x >> README"));
    git(
        &fx.repo.root,
        &["update-index", "--skip-worktree", "README"],
    );
    let out = fx.check(&[]);
    assert_eq!(code(&out), 1, "{}", stdout(&out));
    assert!(
        stdout(&out).contains("the tree changed during the run (README)"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn a_relative_or_empty_path_entry_is_refused_with_mbx() {
    // Finding 5.
    let fx = Fx::new("");
    fx.fake_mbx("1.30.0");
    fx.set_settings(&fx.mbx_settings(""));
    for first in ["bin", "", "."] {
        let path = format!("{first}:{}", fx.path().to_string_lossy());
        let out = fx.check_env(&[], &[("PATH", &path)]);
        assert_eq!(
            code(&out),
            4,
            "{first:?}: {}\n{}",
            stdout(&out),
            stderr(&out)
        );
        assert!(
            stderr(&out).contains("PATH has the relative entry"),
            "{}",
            stderr(&out)
        );
    }
}

#[test]
fn another_target_folder_than_the_workspaces_is_refused_with_mbx() {
    // Finding 6.
    let fx = Fx::new("");
    fx.fake_mbx("1.30.0");
    fx.set_settings(&fx.mbx_settings(""));
    let elsewhere = fx.repo.tmp.path().join("realtarget");
    let out = fx.check_env(
        &[],
        &[("CARGO_BUILD_TARGET_DIR", elsewhere.to_str().unwrap())],
    );
    assert_eq!(code(&out), 4, "{}\n{}", stdout(&out), stderr(&out));
    assert!(
        stderr(&out).contains("CARGO_BUILD_TARGET_DIR is set"),
        "{}",
        stderr(&out)
    );
    assert!(!elsewhere.exists());

    fx.repo.write(
        ".cargo/config.toml",
        &format!("[build]\ntarget-dir = {:?}\n", elsewhere.to_str().unwrap()),
    );
    fx.repo.commit_all("target-dir");
    let out = fx.check(&[]);
    assert_eq!(code(&out), 4, "{}\n{}", stdout(&out), stderr(&out));
    assert!(
        stderr(&out).contains("cargo builds in") && stderr(&out).contains("realtarget"),
        "{}",
        stderr(&out)
    );
    assert!(!elsewhere.exists(), "nothing was built");
}

#[test]
fn a_target_folder_moved_during_the_run_is_red() {
    let fx = Fx::new("");
    ignore(&fx, ".cargo\n");
    let storage = fx.fake_mbx("1.30.0");
    let elsewhere = fx.repo.tmp.path().join("realtarget");
    let step = shell_step(
        "late",
        &format!(
            "mkdir -p .cargo && printf '[build]\\ntarget-dir = \"{}\"\\n' > .cargo/config.toml",
            elsewhere.display()
        ),
    );
    fx.set_settings(&format!("{}{step}", fx.mbx_settings("")));
    fs::create_dir_all(storage.join("t1")).unwrap();
    std::os::unix::fs::symlink(storage.join("t1"), fx.repo.root.join("target")).unwrap();
    let out = fx.check(&[]);
    assert_eq!(code(&out), 1, "{}\n{}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains("cargo builds in"), "{}", stdout(&out));
    assert!(
        last_line(&out).contains("failed: mbx;"),
        "{}",
        last_line(&out)
    );
}

#[test]
fn a_nextest_file_written_during_the_run_is_red() {
    // Finding 7: a hook writes an ignored file with a default filter.
    let hook = "[[check.prepare]]\nname = \"filter\"\nrun = [\"sh\", \"-c\", \"mkdir -p .config && printf '[profile.default]\\\\ndefault-filter = \\\"not test(adds)\\\"\\\\n' > .config/nextest.toml\"]\n";
    let fx = Fx::new(hook);
    ignore(&fx, ".config\n");
    let out = fx.check(&[]);
    assert_eq!(code(&out), 1, "{}\n{}", stdout(&out), stderr(&out));
    assert!(
        stdout(&out).contains(".config/nextest.toml appeared during the run"),
        "{}",
        stdout(&out)
    );
    assert!(
        last_line(&out).contains("failed: nextest-config;"),
        "{}",
        last_line(&out)
    );

    // The same for a file that changes, and for nextest's user config.
    let user = fx.repo.tmp.path().join("user-nextest.toml");
    let fx = Fx::new(&shell_step(
        "late",
        &format!("echo '# changed' > {}", user.display()),
    ));
    let out = fx.check_env(&[], &[("NEXTEST_USER_CONFIG_FILE", user.to_str().unwrap())]);
    assert_eq!(code(&out), 1, "{}\n{}", stdout(&out), stderr(&out));
    assert!(
        stdout(&out).contains("user-nextest.toml appeared during the run"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn an_exclusion_applies_only_where_nextest_accepts_it() {
    // Finding 8: `package(b)` exists in the second workspace only.
    let fx = Fx::new("");
    let r = &fx.repo;
    r.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"a\"]\nexclude = [\"w2\"]\nresolver = \"3\"\n",
    );
    r.write(
        "w2/Cargo.toml",
        "[workspace]\nmembers = [\"b\"]\nresolver = \"3\"\n",
    );
    r.write(
        "w2/b/Cargo.toml",
        "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    r.write(
        "w2/b/src/lib.rs",
        "#[cfg(test)]\nmod tests {\n    #[test]\n    fn fine() {}\n\n    #[test]\n    fn skip_me() {\n        panic!(\"excluded\");\n    }\n}\n",
    );
    let out = fx.run_cmd(
        Command::new("cargo").arg("generate-lockfile"),
        &r.root.join("w2"),
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    fx.set_settings("[[rust.exclude]]\nfilter = \"package(b) & test(skip_me)\"\nreason = \"needs the engine\"\n");
    let text = fs::read_to_string(r.root.join(".atelier/atelier.toml")).unwrap();
    r.write(
        ".atelier/atelier.toml",
        &text.replace("workspaces = [\".\"]", "workspaces = [\".\", \"w2\"]"),
    );
    r.commit_all("two workspaces");

    let out = fx.check(&[]);
    assert_eq!(code(&out), 0, "{}\n{}", stdout(&out), stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("is not applied in workspace ."), "{text}");
    assert!(
        last_line(&out).contains(", 3 tests, 1 excluded, "),
        "{}",
        last_line(&out)
    );

    // An exclusion no workspace accepts is RED and named.
    fx.set_settings("[[rust.exclude]]\nfilter = \"package(nowhere)\"\nreason = \"typo\"\n");
    let text = fs::read_to_string(r.root.join(".atelier/atelier.toml")).unwrap();
    r.write(
        ".atelier/atelier.toml",
        &text.replace("workspaces = [\".\"]", "workspaces = [\".\", \"w2\"]"),
    );
    r.commit_all("a typo");
    let out = fx.check(&[]);
    assert_eq!(code(&out), 1, "{}\n{}", stdout(&out), stderr(&out));
    assert!(
        stdout(&out).contains(
            "exclusion package(nowhere) (typo) is not accepted by nextest in any workspace"
        ),
        "{}",
        stdout(&out)
    );
    assert!(
        last_line(&out).contains("exclusions"),
        "{}",
        last_line(&out)
    );
}

#[test]
fn an_unreadable_nextest_file_is_refused() {
    // Finding 9.
    let fx = Fx::new("");
    fs::create_dir_all(fx.repo.root.join(".config")).unwrap();
    fs::write(
        fx.repo.root.join(".config/nextest.toml"),
        [0xff, 0xfe, 0x00],
    )
    .unwrap();
    fx.repo.commit_all("binary nextest file");
    let out = fx.check(&[]);
    assert_eq!(code(&out), 4, "{}\n{}", stdout(&out), stderr(&out));
    assert!(
        stderr(&out).contains(".config/nextest.toml: cannot be read"),
        "{}",
        stderr(&out)
    );
    assert!(fx.runs().is_empty());
}

/// The background process of the prerequisite test below: macOS hides the environment of Apple's
/// own programs, so the sleeper is this test binary, as in the run tests.
#[test]
fn sleeper_probe_entry() {
    if std::env::var("ATELIER_TEST_PROBE").is_ok() {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

#[test]
fn a_prerequisite_command_that_leaves_a_process_behind_is_refused() {
    // Finding 10.
    let fx = Fx::new("");
    let pid_file = fx.repo.tmp.path().join("sleeper.pid");
    let script = format!(
        "ATELIER_TEST_PROBE=1 {} sleeper_probe_entry --exact --nocapture --test-threads=1 >/dev/null 2>&1 </dev/null & echo $! > {}; echo v1",
        std::env::current_exe().unwrap().display(),
        pid_file.display()
    );
    let tool = format!(
        "[[prerequisites.tools]]\nname = \"bg\"\nrun = [\"sh\", \"-c\", {script:?}]\nexpect = \"v1\"\n"
    );
    fx.set_settings(&tool);
    let out = fx.check(&[]);
    // Whatever happens, do not leave the sleeper running.
    if let Ok(pid) = fs::read_to_string(&pid_file) {
        let _ = Command::new("kill").arg(pid.trim()).status();
    }
    assert_eq!(code(&out), 4, "{}\n{}", stdout(&out), stderr(&out));
    assert!(
        stderr(&out).contains("a prerequisite command left processes behind"),
        "{}",
        stderr(&out)
    );
    assert!(fx.runs().is_empty());
}
