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
use atelier::runs::{Outcome, Run, RunInfo, RunKind, RunResult, STEP_TRAMPOLINE, StepStatus};
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
    steps_use_atelier();
    let code = match mode.as_str() {
        "run" => probe_run(),
        "escape" => probe_escape(),
        "sleeper" => {
            sleep(Duration::from_secs(60));
            0
        }
        "nested" => probe_nested(),
        "after-run" => probe_after_run(),
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
        RunKind::Check,
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

/// An atelier inside a step of another: runs one step that leaves a survivor, then exits.
fn probe_nested() -> i32 {
    let state_root = PathBuf::from(std::env::var("ATELIER_TEST_STATE").unwrap());
    let state = State::open_at(state_root).unwrap();
    let mut run = Run::start(
        &state,
        LockKey::branch("inner"),
        RunKind::Gate,
        "inner",
        Path::new("/"),
    )
    .unwrap();
    let me = std::env::current_exe().unwrap();
    run.step(
        "escape",
        &[
            me.to_string_lossy().into_owned(),
            "probe_child_entry".into(),
            "--exact".into(),
            "--nocapture".into(),
            "--test-threads=1".into(),
        ],
        Path::new("/"),
        &[(PROBE.into(), "escape".into())],
    )
    .unwrap();
    let result = run.finish(true).unwrap();
    fs::write(
        std::env::var("ATELIER_TEST_INNER").unwrap(),
        format!("{}\n{}", result.id, result.survivors.join("\n")),
    )
    .unwrap();
    0
}

/// Finishes a run, says so, then waits: a stop signal must end it as it would any program.
fn probe_after_run() -> i32 {
    let state_root = PathBuf::from(std::env::var("ATELIER_TEST_STATE").unwrap());
    let flag = PathBuf::from(std::env::var("ATELIER_TEST_PIDFILE").unwrap());
    let state = State::open_at(state_root).unwrap();
    let mut run = Run::start(
        &state,
        LockKey::branch("after"),
        RunKind::Check,
        "after",
        Path::new("/"),
    )
    .unwrap();
    run.step("t", &["/usr/bin/true".into()], Path::new("/"), &[])
        .unwrap();
    run.finish(true).unwrap();
    fs::write(&flag, "finished").unwrap();
    sleep(Duration::from_secs(30));
    fs::write(&flag, "still alive").unwrap();
    0
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

/// `Run::start`, retried for a moment while the lock looks held by another atelier. On Linux a
/// child that another test thread is spawning holds a copy of every descriptor of this process
/// until it execs (glibc's `posix_spawn` clones the descriptor table), so a lock this test has
/// just released can look held for a few milliseconds. atelier itself spawns from one thread,
/// and that spawn returns only once the child has exec'd.
fn start_after_release(
    state: &State,
    key: LockKey,
    kind: RunKind,
    command: &str,
    worktree: &Path,
) -> atelier::exit::Result<Run> {
    steps_use_atelier();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match Run::start(state, key.clone(), kind.clone(), command, worktree) {
            Err(e)
                if e.message.contains("held by another atelier command")
                    && Instant::now() < deadline =>
            {
                sleep(Duration::from_millis(10));
            }
            other => return other,
        }
    }
}

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
    let mut run = start_after_release(
        &state,
        LockKey::worktree(&checkout),
        RunKind::Check,
        "probe",
        &repo.root,
    )
    .unwrap();
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
    let mut run = start_after_release(
        &state,
        LockKey::branch("x"),
        RunKind::Check,
        "probe",
        &repo.root,
    )
    .unwrap();
    assert_eq!(
        run.step("bad", &sh("exit 3"), &repo.root, &[])
            .unwrap()
            .exit,
        Some(3)
    );
    let missing = run
        .step("missing", &["/nonexistent/program".into()], &repo.root, &[])
        .unwrap();
    // The shell that starts every step reports the missing program (126 on macOS, 127 with dash).
    assert_eq!(missing.status, StepStatus::Failed);
    assert!(matches!(missing.exit, Some(126 | 127)), "{missing:?}");
    let log = fs::read_to_string(run.dir().join("missing.log")).unwrap();
    assert!(log.contains("/nonexistent/program"), "{log}");
    let result = run.finish(true).unwrap();
    assert_eq!(result.outcome, Outcome::Failed);
    assert!(result.failures.iter().any(|f| f == "bad failed"));
    assert!(result.failures.iter().any(|f| f == "missing failed"));
}

#[test]
fn a_step_never_runs_unless_its_group_was_recorded() {
    // The trampoline every step starts with, released by nothing: atelier died between
    // starting the step and recording its group. The program must not run.
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let mut child = Command::new("/bin/sh")
        .args(["-c", STEP_TRAMPOLINE, "atelier-step"])
        .args(["/usr/bin/touch", marker.to_str().unwrap()])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdin.take());
    assert_eq!(wait_child(&mut child, Duration::from_secs(10)), 125);
    assert!(!marker.exists());
    // Released, it runs the program.
    let mut child = Command::new("/bin/sh")
        .args(["-c", STEP_TRAMPOLINE, "atelier-step"])
        .args(["/usr/bin/touch", marker.to_str().unwrap()])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), b"go\n").unwrap();
    drop(child.stdin.take());
    assert_eq!(wait_child(&mut child, Duration::from_secs(10)), 0);
    assert!(marker.exists());
}

#[test]
fn a_step_that_leaves_its_group_populated_fails_and_the_group_is_stopped() {
    let repo = Repo::new();
    let state = State::open(&repo.checkout()).unwrap();
    let pidfile = repo.tmp.path().join("bg.pid");
    let mut run = start_after_release(
        &state,
        LockKey::branch("x"),
        RunKind::Check,
        "probe",
        &repo.root,
    )
    .unwrap();
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
    let run = start_after_release(
        &state,
        LockKey::branch("x"),
        RunKind::Check,
        "probe",
        &repo.root,
    )
    .unwrap();
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

/// Linux: the marker scan. On macOS the coalition catches this at the end of the step
/// (`tests/launchd.rs`).
#[cfg(not(target_os = "macos"))]
#[test]
fn a_survivor_that_left_its_group_makes_the_run_red_and_blocks_the_next_run() {
    let repo = Repo::new();
    let state = State::open(&repo.checkout()).unwrap();
    let pidfile = repo.tmp.path().join("sleeper.pid");
    let me = std::env::current_exe().unwrap();
    let mut run = start_after_release(
        &state,
        LockKey::branch("x"),
        RunKind::Check,
        "probe",
        &repo.root,
    )
    .unwrap();
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
    let e = start_after_release(
        &state,
        LockKey::branch("x"),
        RunKind::Check,
        "probe",
        &repo.root,
    )
    .unwrap_err();
    assert_eq!(e.exit, Exit::Refused);
    assert!(e.message.contains(&sleeper.to_string()), "{}", e.message);
    assert!(
        e.message.contains(&format!("ATELIER_RUN={id}")),
        "{}",
        e.message
    );
    // Another lock is not affected.
    start_after_release(
        &state,
        LockKey::branch("y"),
        RunKind::Check,
        "probe",
        &repo.root,
    )
    .unwrap()
    .finish(true)
    .unwrap();

    kill_and_wait(sleeper, false);
    let run = start_after_release(
        &state,
        LockKey::branch("x"),
        RunKind::Check,
        "probe",
        &repo.root,
    )
    .unwrap();
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
    let e = Run::start(
        &state,
        LockKey::branch("probe/branch"),
        RunKind::Check,
        "probe",
        &repo.root,
    )
    .unwrap_err();
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
    // The group is recorded with its leader's start time (and, on macOS, the step job's
    // coalition after it).
    let record = fs::read_to_string(state.locks_dir().join("branch/probe%2Fbranch.procs")).unwrap();
    let start = atelier::procs::start_time(step).unwrap();
    assert!(
        record
            .lines()
            .any(|l| l.starts_with(&format!("group {step} "))
                && l.split(' ').nth(3) == Some(start.as_str())),
        "{record}"
    );
    #[cfg(target_os = "macos")]
    assert!(
        record
            .lines()
            .any(|l| l.split(' ').nth(4).is_some_and(|c| c.contains('@'))),
        "{record}"
    );

    let e = start_after_release(
        &state,
        LockKey::branch("probe/branch"),
        RunKind::Check,
        "probe",
        &repo.root,
    )
    .unwrap_err();
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
    let run = start_after_release(
        &state,
        LockKey::branch("probe/branch"),
        RunKind::Check,
        "probe",
        &repo.root,
    )
    .unwrap();
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
        let run = start_after_release(
            &state,
            LockKey::branch("probe/branch"),
            RunKind::Check,
            "probe",
            &repo.root,
        )
        .unwrap();
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
    // The next taker clears the record (and, on macOS, removes the dead run's job).
    start_after_release(
        &state,
        LockKey::branch("probe/branch"),
        RunKind::Check,
        "probe",
        &repo.root,
    )
    .unwrap()
    .finish(true)
    .unwrap();
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn escape_argv() -> Vec<String> {
    let me = std::env::current_exe().unwrap();
    vec![
        me.to_string_lossy().into_owned(),
        "probe_child_entry".into(),
        "--exact".into(),
        "--nocapture".into(),
        "--test-threads=1".into(),
    ]
}

#[test]
fn run_files_name_the_kind_the_lock_key_and_where_the_run_ran() {
    let repo = Repo::new();
    let state = State::open(&repo.checkout()).unwrap();
    let mut run = start_after_release(
        &state,
        LockKey::branch("a/b"),
        RunKind::TestNarrow,
        "test",
        &repo.root,
    )
    .unwrap();
    let id = run.id().to_owned();
    let dir = run.dir().to_path_buf();
    let info = RunInfo::read(&dir).unwrap().unwrap();
    assert_eq!(info.kind, RunKind::TestNarrow);
    assert_eq!(info.lock, "branch/a%2Fb");
    assert_eq!(LockKey::from_id(&info.lock), Some(LockKey::branch("a/b")));
    assert_eq!(info.branch.as_deref(), Some("main"));
    assert_eq!(info.atelier_version, VERSION);
    assert_eq!(
        info.pid_start,
        atelier::procs::start_time(std::process::id() as i32)
    );
    assert_eq!(info.runs.last(), Some(&id));
    run.set_tests(atelier::runs::TestCounts {
        passed: 7,
        failed: 1,
        flaky: 1,
        skipped: 2,
        excluded: 3,
    });
    run.set_test_results("junit.xml");
    let result = run.finish(true).unwrap();
    assert_eq!(result.kind, RunKind::TestNarrow);
    assert_eq!(result.branch.as_deref(), Some("main"));
    assert_eq!(result.worktree, repo.root);
    assert_eq!(result.atelier_version, VERSION);
    assert_eq!(
        result.platform,
        atelier::platform::Platform::current().map(|p| p.to_string())
    );
    assert_eq!(result.machine, None);
    assert_eq!(result.tests.unwrap().excluded, 3);
    assert_eq!(result.test_results.as_deref(), Some("junit.xml"));
    assert_eq!(RunResult::read(&dir).unwrap().unwrap(), result);

    // A later version's fields and kinds are read, not refused.
    for file in ["result", "run.toml"] {
        let text = fs::read_to_string(dir.join(file)).unwrap();
        let text = text.replace("kind = \"test-narrow\"", "kind = \"bench\"");
        fs::write(dir.join(file), format!("later_field = 1\n{text}")).unwrap();
    }
    let read = RunResult::read(&dir).unwrap().unwrap();
    assert_eq!(read.kind, RunKind::Other("bench".into()));
    assert_eq!(read.tests, result.tests);
    assert_eq!(
        RunInfo::read(&dir).unwrap().unwrap().kind,
        RunKind::Other("bench".into())
    );
    // A file that is not TOML is refused.
    fs::write(dir.join("result"), "not = = toml").unwrap();
    assert_eq!(RunResult::read(&dir).unwrap_err().exit, Exit::Refused);
}

#[test]
fn a_step_environment_cannot_replace_the_run_markers() {
    let repo = Repo::new();
    let state = State::open(&repo.checkout()).unwrap();
    let mut run = start_after_release(
        &state,
        LockKey::branch("x"),
        RunKind::Check,
        "probe",
        &repo.root,
    )
    .unwrap();
    let id = run.id().to_owned();
    for var in ["ATELIER_RUN", "ATELIER_RUNS"] {
        let e = run
            .step(
                "env",
                &sh("true"),
                &repo.root,
                &[(var.into(), "other".into())],
            )
            .unwrap_err();
        assert_eq!(e.exit, Exit::Usage, "{var}");
        assert!(e.message.contains(var), "{}", e.message);
    }
    let step = run
        .step(
            "env",
            &sh("echo \"run=$ATELIER_RUN runs=$ATELIER_RUNS\""),
            &repo.root,
            &[],
        )
        .unwrap();
    assert_eq!(step.status, StepStatus::Passed);
    let log = fs::read_to_string(run.dir().join("env.log")).unwrap();
    assert!(log.contains(&format!("run={id} ")), "{log}");
    let runs = log.split("runs=").nth(1).unwrap().trim();
    assert_eq!(runs.split(':').next_back(), Some(id.as_str()), "{log}");
    assert_eq!(run.steps().len(), 1, "refused steps are not recorded");
    run.finish(true).unwrap();
}

#[test]
fn a_program_name_starting_with_a_dash_is_refused() {
    let repo = Repo::new();
    let state = State::open(&repo.checkout()).unwrap();
    let mut run = start_after_release(
        &state,
        LockKey::branch("x"),
        RunKind::Check,
        "probe",
        &repo.root,
    )
    .unwrap();
    for argv in [
        vec!["-x".to_owned()],
        vec!["-c".to_owned(), "true".to_owned()],
    ] {
        let e = run.step("dash", &argv, &repo.root, &[]).unwrap_err();
        assert_eq!(e.exit, Exit::Usage, "{argv:?}");
    }
    assert!(run.steps().is_empty());
    run.finish(true).unwrap();
}

/// Linux: the marker scan. On macOS the coalition catches this at the end of the step
/// (`tests/launchd.rs`).
#[cfg(not(target_os = "macos"))]
#[test]
fn an_outer_run_finds_what_a_nested_atelier_left_behind() {
    let repo = Repo::new();
    let state = State::open(&repo.checkout()).unwrap();
    let pidfile = repo.tmp.path().join("sleeper.pid");
    let inner_file = repo.tmp.path().join("inner");
    let mut run =
        start_after_release(&state, LockKey::Gate, RunKind::Gate, "outer", &repo.root).unwrap();
    let outer = run.id().to_owned();
    let step = run
        .step(
            "nested",
            &escape_argv(),
            &repo.root,
            &[
                (PROBE.into(), "nested".into()),
                (
                    "ATELIER_TEST_STATE".into(),
                    state.root().to_string_lossy().into_owned(),
                ),
                (
                    "ATELIER_TEST_PIDFILE".into(),
                    pidfile.to_string_lossy().into_owned(),
                ),
                (
                    "ATELIER_TEST_INNER".into(),
                    inner_file.to_string_lossy().into_owned(),
                ),
            ],
        )
        .unwrap();
    assert_eq!(step.status, StepStatus::Passed, "{step:?}");
    let sleeper = read_pid(&pidfile);
    // The inner run saw its own survivor.
    let inner = fs::read_to_string(&inner_file).unwrap();
    let inner_id = inner.lines().next().unwrap().to_owned();
    assert!(
        inner.contains(&format!("\n{sleeper} ")),
        "inner survivors: {inner}"
    );
    let inner_info = RunInfo::read(&state.runs_dir().join(&inner_id))
        .unwrap()
        .unwrap();
    assert!(
        inner_info
            .runs
            .ends_with(&[outer.clone(), inner_id.clone()])
    );
    // The outer run sees it too, through ATELIER_RUNS.
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
    // Both locks refuse while it lives.
    for key in [LockKey::Gate, LockKey::branch("inner")] {
        let e = start_after_release(&state, key.clone(), RunKind::Check, "probe", &repo.root)
            .unwrap_err();
        assert!(
            e.message.contains(&format!("{sleeper} ")),
            "{key:?}: {}",
            e.message
        );
    }
    kill_and_wait(sleeper, false);
    start_after_release(&state, LockKey::Gate, RunKind::Gate, "probe", &repo.root)
        .unwrap()
        .finish(true)
        .unwrap();
}

fn failing_scan(_: &[String]) -> std::io::Result<Vec<(atelier::procs::ProcInfo, String)>> {
    Err(std::io::Error::other("process list unavailable"))
}

#[test]
fn a_failed_survivor_scan_keeps_the_run_recorded() {
    let repo = Repo::new();
    let state = State::open(&repo.checkout()).unwrap();
    let run = start_after_release(
        &state,
        LockKey::branch("x"),
        RunKind::Check,
        "probe",
        &repo.root,
    )
    .unwrap()
    .with_survivor_scan(failing_scan);
    let id = run.id().to_owned();
    let result = run.finish(true).unwrap();
    assert_eq!(result.outcome, Outcome::Failed);
    assert!(
        result
            .failures
            .iter()
            .any(|f| f.contains("cannot scan for processes left behind")),
        "{:?}",
        result.failures
    );
    let record = fs::read_to_string(state.locks_dir().join("branch/x.procs")).unwrap();
    assert!(record.contains(&format!("run {id}")), "{record}");
    // The next taker scans for it (nothing is left) and clears it.
    start_after_release(
        &state,
        LockKey::branch("x"),
        RunKind::Check,
        "probe",
        &repo.root,
    )
    .unwrap()
    .finish(true)
    .unwrap();
    assert!(!state.locks_dir().join("branch/x.procs").exists());
}

#[test]
fn stop_signals_end_the_process_again_after_a_run() {
    use std::os::unix::process::ExitStatusExt;
    for ignore_hup in [false, true] {
        let repo = Repo::new();
        let state = State::open(&repo.checkout()).unwrap();
        let flag = repo.tmp.path().join("flag");
        let mut cmd = probe("after-run");
        cmd.env("ATELIER_TEST_STATE", state.root())
            .env("ATELIER_TEST_PIDFILE", &flag);
        if ignore_hup {
            // As under nohup: HUP ignored before atelier starts stays ignored.
            // SAFETY: signal is async-signal-safe.
            unsafe {
                cmd.pre_exec(|| {
                    libc::signal(libc::SIGHUP, libc::SIG_IGN);
                    Ok(())
                });
            }
        }
        let mut child = cmd.spawn().unwrap();
        wait_for("the run to finish", Duration::from_secs(20), || {
            fs::read_to_string(&flag).is_ok_and(|s| s == "finished")
        });
        let sig = if ignore_hup {
            // SAFETY: the probe is this test's child.
            unsafe { libc::kill(child.id() as i32, libc::SIGHUP) };
            sleep(Duration::from_millis(300));
            assert!(child.try_wait().unwrap().is_none(), "HUP stays ignored");
            libc::SIGINT
        } else {
            libc::SIGTERM
        };
        // SAFETY: the probe is this test's child.
        unsafe { libc::kill(child.id() as i32, sig) };
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            assert!(Instant::now() < deadline, "the signal was swallowed");
            sleep(Duration::from_millis(20));
        };
        assert_eq!(status.signal(), Some(sig), "{status:?}");
        assert_eq!(fs::read_to_string(&flag).unwrap(), "finished");
    }
}
