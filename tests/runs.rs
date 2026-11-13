//! Runs, locks, process groups, the survivor scan and stop signals (r4 §3.1).
//!
//! Cases that need a separate atelier process (one that is killed, or signalled) re-run this
//! test binary as a probe child: `probe_child_entry` does nothing unless `ATELIER_TEST_PROBE`
//! names a mode.

mod common;

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use atelier::exit::Exit;
use atelier::locks::LockKey;
use atelier::runs::{Outcome, Run, RunResult, StepStatus};
use atelier::state::State;
use atelier::tree;
use common::*;

const PROBE: &str = "ATELIER_TEST_PROBE";

// ----- the probe child -----

#[test]
fn probe_child_entry() {
    let Ok(mode) = std::env::var(PROBE) else {
        return;
    };
    let code = match mode.as_str() {
        "run" => probe_run(),
        "escape" => probe_escape(),
        "sleeper" => {
            sleep(Duration::from_secs(60));
            0
        }
        other => panic!("unknown probe mode {other}"),
    };
    std::process::exit(code);
}

/// Holds a branch lock and runs one shell step; the exit code reflects the run.
fn probe_run() -> i32 {
    let state_root = PathBuf::from(std::env::var("ATELIER_TEST_STATE").unwrap());
    let script = std::env::var("ATELIER_TEST_STEP").unwrap();
    let state = State::open_at(state_root).unwrap();
    let run = match Run::start(
        &state,
        LockKey::branch("probe/branch"),
        "probe",
        Path::new("/"),
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}", e.message);
            return e.exit.code();
        }
    };
    let mut run = run.with_grace(Duration::from_millis(1500));
    fs::write(run.dir().join("probe-started"), "").unwrap();
    run.step(
        "work",
        &["/bin/sh".into(), "-c".into(), script],
        Path::new("/"),
        &[],
    )
    .unwrap();
    let result = run.finish(true).unwrap();
    match result.outcome {
        Outcome::Passed => 0,
        Outcome::Failed | Outcome::Interrupted => 1,
    }
}

/// Starts a sleeper in a new session (it leaves the step's group, keeping the environment),
/// writes its pid, and exits at once.
#[allow(clippy::zombie_processes)] // The sleeper must outlive this process: that is the case tested.
fn probe_escape() -> i32 {
    let pidfile = std::env::var("ATELIER_TEST_PIDFILE").unwrap();
    let mut cmd = Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "probe_child_entry",
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(PROBE, "sleeper")
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().unwrap();
    fs::write(pidfile, child.id().to_string()).unwrap();
    0
}

fn probe(mode: &str) -> Command {
    let mut c = Command::new(std::env::current_exe().unwrap());
    c.args([
        "probe_child_entry",
        "--exact",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(PROBE, mode)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    c
}

// ----- helpers -----

fn alive(pid: i32) -> bool {
    // SAFETY: existence check only.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_for(what: &str, within: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(20));
    }
}

fn read_pid(path: &Path) -> i32 {
    wait_for("a pid file", Duration::from_secs(20), || {
        fs::read_to_string(path).is_ok_and(|s| !s.trim().is_empty())
    });
    fs::read_to_string(path).unwrap().trim().parse().unwrap()
}

/// Stops a process this test started (directly or through a probe) and waits until it is gone.
fn kill_and_wait(pid: i32, group: bool) {
    // SAFETY: the test's own processes only.
    unsafe {
        libc::kill(if group { -pid } else { pid }, libc::SIGKILL);
    }
    wait_for("the test's process to end", Duration::from_secs(10), || {
        !alive(pid)
    });
}

fn only_run_dir(state: &State) -> PathBuf {
    let dirs: Vec<PathBuf> = fs::read_dir(state.runs_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(dirs.len(), 1, "{dirs:?}");
    dirs.into_iter().next().unwrap()
}

fn sh(script: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), script.into()]
}

fn wait_child(child: &mut Child, within: Duration) -> i32 {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status.code().unwrap_or(-1);
        }
        assert!(Instant::now() < deadline, "probe did not exit");
        sleep(Duration::from_millis(20));
    }
}

// ----- tests -----

#[test]
fn a_step_runs_as_its_own_group_leader_with_the_run_marker() {
    let repo = Repo::new();
    let checkout = repo.checkout();
    let state = State::open(&checkout).unwrap();
    let mut run = Run::start(&state, LockKey::worktree(&checkout), "probe", &repo.root).unwrap();
    let tree = tree::tree_id(&checkout).unwrap();
    let head_tree = tree::head_tree(&checkout).unwrap().unwrap();
    run.set_tree(
        tree.clone(),
        checkout.head_commit().unwrap(),
        tree != head_tree,
    );
    let step = run
        .step(
            "env",
            &sh("echo \"pid=$$ pgid=$(ps -o pgid= -p $$ | tr -d ' ') run=$ATELIER_RUN dispatched=${ATELIER_DISPATCHED-unset} extra=$EXTRA\""),
            &repo.root,
            &[("EXTRA".into(), "x".into())],
        )
        .unwrap();
    assert_eq!(step.status, StepStatus::Passed);
    let id = run.id().to_owned();
    let dir = run.dir().to_path_buf();
    let log = fs::read_to_string(dir.join("env.log")).unwrap();
    let pid = log.split("pid=").nth(1).unwrap().split(' ').next().unwrap();
    let pgid = log
        .split("pgid=")
        .nth(1)
        .unwrap()
        .split(' ')
        .next()
        .unwrap();
    assert_eq!(pid, pgid, "{log}");
    assert!(log.contains(&format!("run={id} ")), "{log}");
    assert!(log.contains("dispatched=unset"), "{log}");
    assert!(log.contains("extra=x"), "{log}");

    let result = run.finish(true).unwrap();
    assert_eq!(result.outcome, Outcome::Passed);
    assert_eq!(RunResult::read(&dir).unwrap().unwrap(), result);
    assert_eq!(result.tree.as_deref(), Some(tree.as_str()));
    assert_eq!(result.uncommitted, Some(false));
    assert!(dir.join("run.toml").is_file());
    // Nothing remains recorded under the lock.
    assert!(
        fs::read_dir(state.locks_dir().join("worktree"))
            .unwrap()
            .all(|e| { e.unwrap().path().extension().is_some_and(|x| x == "lock") })
    );
}

#[test]
fn a_failing_or_missing_program_fails_the_run() {
    let repo = Repo::new();
    let state = State::open(&repo.checkout()).unwrap();
    let mut run = Run::start(&state, LockKey::branch("x"), "probe", &repo.root).unwrap();
    assert_eq!(
        run.step("bad", &sh("exit 3"), &repo.root, &[])
            .unwrap()
            .exit,
        Some(3)
    );
    let missing = run
        .step("missing", &["/nonexistent/program".into()], &repo.root, &[])
        .unwrap();
    assert_eq!(missing.status, StepStatus::NotStarted);
    let result = run.finish(true).unwrap();
    assert_eq!(result.outcome, Outcome::Failed);
    assert!(result.failures.iter().any(|f| f == "bad failed"));
    assert!(
        result
            .failures
            .iter()
            .any(|f| f == "missing could not start")
    );
}

#[test]
fn a_step_that_leaves_its_group_populated_fails_and_the_group_is_stopped() {
    let repo = Repo::new();
    let state = State::open(&repo.checkout()).unwrap();
    let pidfile = repo.tmp.path().join("bg.pid");
    let mut run = Run::start(&state, LockKey::branch("x"), "probe", &repo.root).unwrap();
    let step = run
        .step(
            "bg",
            &sh(&format!(
                "sleep 30 & echo $! > {}; exit 0",
                pidfile.display()
            )),
            &repo.root,
            &[],
        )
        .unwrap();
    let bg = read_pid(&pidfile);
    assert_eq!(step.status, StepStatus::LeftProcesses);
    assert_eq!(step.exit, Some(0));
    assert!(
        step.left
            .iter()
            .any(|l| l.starts_with(&format!("{bg} ")) && l.contains("sleep")),
        "{:?}",
        step.left
    );
    assert!(!alive(bg), "the group member was stopped");
    let result = run.finish(true).unwrap();
    assert_eq!(result.outcome, Outcome::Failed);
    assert!(
        result
            .failures
            .iter()
            .any(|f| f.contains("left processes behind")),
        "{:?}",
        result.failures
    );
}

#[test]
fn a_member_that_ignores_term_is_killed() {
    let repo = Repo::new();
    let state = State::open(&repo.checkout()).unwrap();
    let pidfile = repo.tmp.path().join("bg.pid");
    let run = Run::start(&state, LockKey::branch("x"), "probe", &repo.root).unwrap();
    let mut run = run.with_grace(Duration::from_millis(300));
    let started = Instant::now();
    let step = run
        .step(
            "stubborn",
            &sh(&format!(
                "trap '' TERM; sleep 30 & echo $! > {}; exit 0",
                pidfile.display()
            )),
            &repo.root,
            &[],
        )
        .unwrap();
    let bg = read_pid(&pidfile);
    assert_eq!(step.status, StepStatus::LeftProcesses);
    assert!(!alive(bg));
    assert!(step.unstoppable.is_empty());
    assert!(started.elapsed() < Duration::from_secs(10));
    run.finish(true).unwrap();
}

#[test]
fn a_survivor_that_left_its_group_makes_the_run_red_and_blocks_the_next_run() {
    let repo = Repo::new();
    let state = State::open(&repo.checkout()).unwrap();
    let pidfile = repo.tmp.path().join("sleeper.pid");
    let me = std::env::current_exe().unwrap();
    let mut run = Run::start(&state, LockKey::branch("x"), "probe", &repo.root).unwrap();
    let id = run.id().to_owned();
    let step = run
        .step(
            "escape",
            &[
                me.to_string_lossy().into_owned(),
                "probe_child_entry".into(),
                "--exact".into(),
                "--nocapture".into(),
                "--test-threads=1".into(),
            ],
            &repo.root,
            &[
                (PROBE.into(), "escape".into()),
                (
                    "ATELIER_TEST_PIDFILE".into(),
                    pidfile.to_string_lossy().into_owned(),
                ),
            ],
        )
        .unwrap();
    let sleeper = read_pid(&pidfile);
    // It left the group, so the step itself is clean.
    assert_eq!(step.status, StepStatus::Passed, "{step:?}");
    let result = run.finish(true).unwrap();
    assert_eq!(result.outcome, Outcome::Failed);
    assert!(
        result
            .survivors
            .iter()
            .any(|s| s.starts_with(&format!("{sleeper} "))),
        "{:?}",
        result.survivors
    );
    // Found, reported, not signalled.
    assert!(alive(sleeper));

    // The next command under the same lock refuses while it lives.
    let e = Run::start(&state, LockKey::branch("x"), "probe", &repo.root).unwrap_err();
    assert_eq!(e.exit, Exit::Refused);
    assert!(e.message.contains(&sleeper.to_string()), "{}", e.message);
    assert!(
        e.message.contains(&format!("ATELIER_RUN={id}")),
        "{}",
        e.message
    );
    // Another lock is not affected.
    Run::start(&state, LockKey::branch("y"), "probe", &repo.root)
        .unwrap()
        .finish(true)
        .unwrap();

    kill_and_wait(sleeper, false);
    let run = Run::start(&state, LockKey::branch("x"), "probe", &repo.root).unwrap();
    assert_eq!(run.finish(true).unwrap().outcome, Outcome::Passed);
}

#[test]
fn a_killed_atelier_leaves_a_live_group_and_the_next_run_refuses() {
    let repo = Repo::new();
    let state = State::open(&repo.checkout()).unwrap();
    let pidfile = repo.tmp.path().join("step.pid");
    let mut child = probe("run")
        .env("ATELIER_TEST_STATE", state.root())
        .env(
            "ATELIER_TEST_STEP",
            format!("echo $$ > {}; exec sleep 30", pidfile.display()),
        )
        .spawn()
        .unwrap();
    let step = read_pid(&pidfile);

    // While the probe holds the lock, another run is refused for that reason.
    let e = Run::start(&state, LockKey::branch("probe/branch"), "probe", &repo.root).unwrap_err();
    assert_eq!(e.exit, Exit::Refused);
    assert!(
        e.message.contains("held by another atelier command"),
        "{}",
        e.message
    );

    // atelier is killed outright: no cleanup, its lock is released, its step's group lives.
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(alive(step));
    let run_dir = only_run_dir(&state);
    assert!(
        !run_dir.join("result").exists(),
        "a killed run has no result"
    );

    let e = Run::start(&state, LockKey::branch("probe/branch"), "probe", &repo.root).unwrap_err();
    assert_eq!(e.exit, Exit::Refused);
    assert!(e.message.contains("still alive"), "{}", e.message);
    assert!(e.message.contains(&format!("{step} ")), "{}", e.message);
    assert!(
        e.message.contains(&format!("group {step}")),
        "{}",
        e.message
    );
    // Never stopped by atelier.
    assert!(alive(step));

    kill_and_wait(step, true);
    let run = Run::start(&state, LockKey::branch("probe/branch"), "probe", &repo.root).unwrap();
    assert_eq!(run.finish(true).unwrap().outcome, Outcome::Passed);
}

#[test]
fn stop_signals_stop_the_group_and_write_a_result() {
    for (sig, name) in [
        (libc::SIGTERM, "SIGTERM"),
        (libc::SIGINT, "SIGINT"),
        (libc::SIGHUP, "SIGHUP"),
    ] {
        let repo = Repo::new();
        let state = State::open(&repo.checkout()).unwrap();
        let pidfile = repo.tmp.path().join("step.pid");
        // The step's own child ignores TERM, so the stop needs KILL too.
        let mut child = probe("run")
            .env("ATELIER_TEST_STATE", state.root())
            .env(
                "ATELIER_TEST_STEP",
                format!(
                    "trap '' TERM HUP INT; sleep 30 & echo $! > {}; wait",
                    pidfile.display()
                ),
            )
            .spawn()
            .unwrap();
        let member = read_pid(&pidfile);
        let started = Instant::now();
        // SAFETY: the probe is this test's child.
        unsafe {
            libc::kill(child.id() as i32, sig);
        }
        let code = wait_child(&mut child, Duration::from_secs(20));
        assert_eq!(code, 1, "{name}");
        assert!(started.elapsed() < Duration::from_secs(15), "{name}");
        assert!(!alive(member), "{name}: the group was stopped");

        let run_dir = only_run_dir(&state);
        let result = RunResult::read(&run_dir).unwrap().expect("a result");
        assert_eq!(result.outcome, Outcome::Interrupted, "{name}");
        assert_eq!(result.signal.as_deref(), Some(name));
        assert_eq!(result.steps[0].status, StepStatus::Interrupted);
        assert!(
            result.steps[0]
                .left
                .iter()
                .any(|l| l.starts_with(&format!("{member} "))),
            "{name}: {:?}",
            result.steps[0].left
        );
        // Nothing stays recorded, so the next run starts.
        let run = Run::start(&state, LockKey::branch("probe/branch"), "probe", &repo.root).unwrap();
        run.finish(true).unwrap();
    }
}

#[test]
fn the_lock_is_not_handed_to_children() {
    // A step that would hold the lock open if it were inherited: atelier is killed, the step
    // lives, and the lock itself is free (the refusal is about processes, not the lock).
    let repo = Repo::new();
    let state = State::open(&repo.checkout()).unwrap();
    let pidfile = repo.tmp.path().join("step.pid");
    let mut child = probe("run")
        .env("ATELIER_TEST_STATE", state.root())
        .env(
            "ATELIER_TEST_STEP",
            format!("echo $$ > {}; exec sleep 30", pidfile.display()),
        )
        .spawn()
        .unwrap();
    let step = read_pid(&pidfile);
    child.kill().unwrap();
    child.wait().unwrap();
    let lock = fs::File::open(state.locks_dir().join("branch/probe%2Fbranch.lock")).unwrap();
    assert!(lock.try_lock().is_ok(), "the step does not hold the lock");
    drop(lock);
    kill_and_wait(step, true);
}
