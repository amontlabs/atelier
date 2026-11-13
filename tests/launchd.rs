//! Steps as launchd jobs on macOS (DESIGN §8.4): the exact environment, exit statuses and
//! output, survivors caught by coalition and stopped, no false positives, a nested atelier, the
//! release handshake and the preflight probe.
//!
//! Cases that need an atelier process of their own re-run this test binary as a probe child:
//! `probe_child_entry` does nothing unless `ATELIER_TEST_PROBE` names a mode.

mod common;

#[cfg(not(target_os = "macos"))]
#[test]
fn launchd_steps_are_macos_only() {
    eprintln!(
        "skipped: launchd jobs and coalitions exist only on macOS; Linux keeps process groups and the marker scan (tests/runs.rs)"
    );
}

#[cfg(target_os = "macos")]
mod mac {
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use atelier::coalition;
    use atelier::exit::Exit;
    use atelier::launchd;
    use atelier::locks::LockKey;
    use atelier::runs::{Outcome, Run, RunInfo, RunKind, StepStatus};
    use atelier::state::State;

    use super::common::*;

    const PROBE: &str = "ATELIER_TEST_PROBE";

    #[test]
    fn probe_child_entry() {
        let Ok(mode) = std::env::var(PROBE) else {
            return;
        };
        if mode == "missing-shim" {
            std::process::exit(probe_missing_shim());
        }
        steps_use_atelier();
        let code = match mode.as_str() {
            "env" => probe_env(),
            "nested-killed" => probe_nested_killed(),
            "die-after-bootstrap" => probe_die_after_bootstrap(),
            "sleep" => {
                sleep(Duration::from_secs(30));
                0
            }
            other => panic!("unknown probe mode {other}"),
        };
        std::process::exit(code);
    }

    /// Writes what the step sees: its environment (NUL-separated, raw bytes), its folder, its
    /// open-file limit and umask.
    fn probe_env() -> i32 {
        let out = PathBuf::from(std::env::var("ATELIER_TEST_OUT").unwrap());
        let mut env = Vec::new();
        for (k, v) in std::env::vars_os() {
            env.extend_from_slice(k.as_bytes());
            env.push(b'=');
            env.extend_from_slice(v.as_bytes());
            env.push(0);
        }
        fs::write(out.join("env"), env).unwrap();
        fs::write(
            out.join("cwd"),
            std::env::current_dir().unwrap().as_os_str().as_bytes(),
        )
        .unwrap();
        fs::write(out.join("nofile"), nofile().to_string()).unwrap();
        fs::write(out.join("umask"), umask().to_string()).unwrap();
        0
    }

    /// An atelier inside a step of another, killed by its own step while that step and a
    /// daemonized sleeper live on.
    fn probe_nested_killed() -> i32 {
        let state =
            State::open_at(PathBuf::from(std::env::var("ATELIER_TEST_STATE").unwrap())).unwrap();
        let pidfile = std::env::var("ATELIER_TEST_PIDFILE").unwrap();
        let mut run = Run::start(
            &state,
            LockKey::branch("inner"),
            RunKind::Check,
            "inner",
            Path::new("/"),
        )
        .unwrap();
        let me = std::process::id();
        fs::write(
            std::env::var("ATELIER_TEST_INNER").unwrap(),
            format!("{} {me}", run.id()),
        )
        .unwrap();
        run.step(
            "work",
            &sh(&format!(
                "{} ; kill -9 {me}; exec /bin/sleep 30",
                daemonize_cmd(&pidfile, false)
            )),
            Path::new("/"),
            &[],
        )
        .unwrap();
        // Not reached: the step kills this process.
        run.finish(true).unwrap();
        0
    }

    /// Starts jobs with a shim that does not exist: the job, then the probe of a new state.
    /// Writes `<seconds> <message>` lines for each to `ATELIER_TEST_OUT`.
    fn probe_missing_shim() -> i32 {
        let out = PathBuf::from(std::env::var("ATELIER_TEST_OUT").unwrap());
        let missing = out.join("no-such-atelier");
        launchd::use_program(&missing);
        let mut lines = Vec::new();
        let label = format!("atelier.test.{}.missing", std::process::id());
        let spec = launchd::Spec {
            argv: vec!["/usr/bin/true".into()],
            cwd: "/".into(),
            env: Vec::new(),
            stdout: out.join("o"),
            stderr: out.join("o"),
        };
        let t = Instant::now();
        let msg = match launchd::launch(&label, &spec, &mut || false) {
            Ok(_) => "started".to_owned(),
            Err(launchd::LaunchError::Unavailable(m)) => format!("unavailable: {m}"),
            Err(e) => format!("{e:?}"),
        };
        lines.push(format!("{:.3} {msg}", t.elapsed().as_secs_f64()));
        lines.push(format!("loaded {}", label_loaded(&label)));
        let state = State::open_at(out.join("state")).unwrap();
        let t = Instant::now();
        let msg = match Run::start(
            &state,
            LockKey::branch("x"),
            RunKind::Check,
            "p",
            Path::new("/"),
        ) {
            Ok(_) => "started".to_owned(),
            Err(e) => e.message,
        };
        lines.push(format!("{:.3} {msg}", t.elapsed().as_secs_f64()));
        fs::write(out.join("result"), lines.join("\n")).unwrap();
        0
    }

    /// Leaves three jobs loaded and dies: a probe-shaped job and a step-shaped job whose `Job`
    /// is forgotten (never booted out), then a job whose launch is cut short by `_exit` right
    /// after its bootstrap, before its shim reports. The step jobs are of run
    /// `ATELIER_TEST_RUN`.
    fn probe_die_after_bootstrap() -> i32 {
        let run = std::env::var("ATELIER_TEST_RUN").unwrap();
        let labels = [
            format!("atelier.probe.{}.0a1b", std::process::id()),
            format!("atelier.{run}.1"),
            format!("atelier.{run}.2"),
        ];
        let spec = launchd::Spec {
            argv: vec!["/usr/bin/true".into()],
            cwd: "/".into(),
            env: Vec::new(),
            stdout: "/dev/null".into(),
            stderr: "/dev/null".into(),
        };
        for l in &labels[..2] {
            let job = launchd::launch(l, &spec, &mut || false).unwrap();
            std::mem::forget(job);
        }
        let _ = launchd::launch(&labels[2], &spec, &mut || {
            // SAFETY: ends this test child at once, as a KILL would.
            unsafe { libc::_exit(9) }
        });
        1
    }

    fn nofile() -> u64 {
        let mut rl = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: a valid out pointer.
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) };
        rl.rlim_cur
    }

    fn umask() -> u32 {
        // SAFETY: umask is set back at once.
        unsafe {
            let m = libc::umask(0o022);
            libc::umask(m);
            u32::from(m)
        }
    }

    fn probe_argv() -> Vec<String> {
        vec![
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "mac::probe_child_entry".into(),
            "--exact".into(),
            "--nocapture".into(),
            "--test-threads=1".into(),
        ]
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }

    /// A shell command that starts `/bin/sleep 30` in a new session after a double fork (with an
    /// empty environment when `env_i`), writes its pid to `pidfile`, and returns once it has.
    fn daemonize_cmd(pidfile: &str, env_i: bool) -> String {
        daemonize_exec(pidfile, env_i, &["/bin/sleep", "30"])
    }

    /// As [`daemonize_cmd`], the daemon running `argv` (no quotes in it).
    fn daemonize_exec(pidfile: &str, env_i: bool, argv: &[&str]) -> String {
        let perl = r#"use POSIX; my $f = shift; defined(my $p = fork) or die; if ($p) { select(undef, undef, undef, 0.01) until -s $f; exit 0 } POSIX::setsid(); defined($p = fork) or die; exit 0 if $p; open(my $h, ">", "$f.tmp") or die; print $h $$; close $h; rename("$f.tmp", $f); exec @ARGV"#;
        format!(
            "{}/usr/bin/perl -e '{perl}' '{pidfile}' {}",
            if env_i { "/usr/bin/env -i " } else { "" },
            argv.iter()
                .map(|a| format!("'{a}'"))
                .collect::<Vec<_>>()
                .join(" ")
        )
    }

    fn start(state: &State, key: LockKey) -> atelier::exit::Result<Run> {
        steps_use_atelier();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match Run::start(state, key.clone(), RunKind::Check, "probe", Path::new("/")) {
                Err(e)
                    if e.message.contains("held by another atelier command")
                        && Instant::now() < deadline =>
                {
                    sleep(Duration::from_millis(10))
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

    fn kill_and_wait(pid: i32) {
        // SAFETY: the test's own process.
        unsafe { libc::kill(pid, libc::SIGKILL) };
        wait_for("the test's process to end", Duration::from_secs(10), || {
            !alive(pid)
        });
    }

    fn label_loaded(label: &str) -> bool {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        ["gui", "user"].iter().any(|d| {
            Command::new("/bin/launchctl")
                .args(["print", &format!("{d}/{uid}/{label}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        })
    }

    // ----- tests -----

    #[test]
    fn a_step_job_sees_exactly_the_requested_environment() {
        let repo = Repo::new();
        let state = State::open(&repo.checkout()).unwrap();
        let out = repo.tmp.path().join("out");
        fs::create_dir(&out).unwrap();
        let mut run = start(&state, LockKey::branch("x")).unwrap();
        let extra = [
            (PROBE.to_owned(), "env".to_owned()),
            (
                "ATELIER_TEST_OUT".to_owned(),
                out.to_string_lossy().into_owned(),
            ),
            ("SPACED".to_owned(), "x y \"q\" <&>\tz".to_owned()),
            ("EMPTY".to_owned(), String::new()),
            ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
        ];
        let step = run.step("env", &probe_argv(), &repo.root, &extra).unwrap();
        assert_eq!(step.status, StepStatus::Passed, "{step:?}");
        let info = RunInfo::read(run.dir()).unwrap().unwrap();

        let mut want: Vec<Vec<u8>> = std::env::vars_os()
            .filter(|(k, _)| {
                ![
                    "ATELIER_DISPATCHED",
                    "ATELIER_RUN_ID",
                    "ATELIER_RUN",
                    "ATELIER_RUNS",
                    "ATELIER_COALITIONS",
                ]
                .iter()
                .any(|x| k == x)
                    && !extra.iter().any(|(e, _)| k == e.as_str())
            })
            .map(|(k, v)| [k.as_bytes(), b"=", v.as_bytes()].concat())
            .collect();
        for (k, v) in &extra {
            want.push(format!("{k}={v}").into_bytes());
        }
        want.push(format!("ATELIER_RUN={}", run.id()).into_bytes());
        want.push(format!("ATELIER_RUNS={}", info.runs.join(":")).into_bytes());
        want.push(
            format!(
                "ATELIER_COALITIONS={}",
                run.dir().join("coalitions").display()
            )
            .into_bytes(),
        );
        want.sort();
        let mut got: Vec<Vec<u8>> = fs::read(out.join("env"))
            .unwrap()
            .split(|b| *b == 0)
            .filter(|e| !e.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
        got.sort();
        let show = |v: &[Vec<u8>]| {
            v.iter()
                .map(|e| String::from_utf8_lossy(e).into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(show(&got), show(&want));
        // launchd's own variables (its service name is the job's label) are gone.
        assert!(
            !got.iter()
                .any(|e| e.starts_with(b"XPC_SERVICE_NAME=atelier."))
        );
        assert_eq!(
            fs::read_to_string(out.join("cwd")).unwrap(),
            repo.root.to_string_lossy()
        );
        assert_eq!(
            fs::read_to_string(out.join("nofile")).unwrap(),
            nofile().to_string(),
            "the caller's open-file limit, not launchd's 256"
        );
        assert_eq!(
            fs::read_to_string(out.join("umask")).unwrap(),
            umask().to_string()
        );
        assert_eq!(run.finish(true).unwrap().outcome, Outcome::Passed);
    }

    #[test]
    fn exit_codes_signals_arguments_and_output_are_exact() {
        let repo = Repo::new();
        let state = State::open(&repo.checkout()).unwrap();
        let mut run = start(&state, LockKey::branch("x")).unwrap();
        let ok = run.step("ok", &sh("exit 0"), &repo.root, &[]).unwrap();
        assert_eq!(
            (ok.status, ok.exit, ok.signal),
            (StepStatus::Passed, Some(0), None)
        );
        for (script, exit, signal) in [
            ("exit 7", Some(7), None),
            ("exit 100", Some(100), None),
            // launchd prints 64 to 78 with their sysexits names (`64: EX_USAGE`).
            ("exit 64", Some(64), None),
            ("exit 75", Some(75), None),
            ("exit 78", Some(78), None),
            ("kill -TERM $$", None, Some(15)),
            ("kill -KILL $$", None, Some(9)),
        ] {
            let t = Instant::now();
            let s = run.step("code", &sh(script), &repo.root, &[]).unwrap();
            assert_eq!(s.status, StepStatus::Failed, "{script}: {s:?}");
            assert_eq!((s.exit, s.signal), (exit, signal), "{script}");
            assert!(
                t.elapsed() < Duration::from_secs(3),
                "{script}: {:?}",
                t.elapsed()
            );
        }
        let argv: Vec<String> = [
            "/bin/sh",
            "-c",
            "printf '%s|' \"$@\"; printf 'to-stderr \\t tab\\n' >&2; printf 'out\\tline\\n'",
            "sh",
            "arg one",
            "<two>",
            "",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let s = run
            .step_with_stdout("io", &argv, &repo.root, &[], Some("io.out"))
            .unwrap();
        assert_eq!(s.status, StepStatus::Passed, "{s:?}");
        assert_eq!(
            fs::read_to_string(run.dir().join("io.out")).unwrap(),
            "arg one|<two>||out\tline\n"
        );
        assert_eq!(
            fs::read_to_string(run.dir().join("io.log")).unwrap(),
            "to-stderr \t tab\n"
        );
        // The job folders are in the user's temporary folder, and gone once each job is booted
        // out; none is in the run folder.
        for n in 1..=9 {
            let label = format!("atelier.{}.{n}", run.id());
            assert!(!launchd::job_dir(&label).exists(), "{label}");
            assert!(!label_loaded(&label), "{label}");
        }
        assert!(!run.dir().join("jobs").exists());
        assert!(launchd::jobs_root().starts_with(launchd::user_temp_dir()));
        let r = run.finish(true).unwrap();
        assert_eq!(r.outcome, Outcome::Failed);
        assert!(r.survivors.is_empty(), "{:?}", r.survivors);
    }

    fn survivor_case(env_i: bool) {
        let repo = Repo::new();
        let state = State::open(&repo.checkout()).unwrap();
        let pidfile = repo.tmp.path().join("sleeper.pid");
        let mut run = start(&state, LockKey::branch("x")).unwrap();
        let started = Instant::now();
        let step = run
            .step(
                "escape",
                &sh(&daemonize_cmd(&pidfile.to_string_lossy(), env_i)),
                &repo.root,
                &[],
            )
            .unwrap();
        let sleeper = read_pid(&pidfile);
        assert_eq!(step.exit, Some(0));
        assert_eq!(step.status, StepStatus::LeftProcesses, "{step:?}");
        assert!(
            step.left
                .iter()
                .any(|l| l.starts_with(&format!("{sleeper} ")) && l.contains("sleep")),
            "{:?}",
            step.left
        );
        assert!(!alive(sleeper), "the survivor was stopped");
        assert!(step.unstoppable.is_empty());
        assert!(started.elapsed() < Duration::from_secs(15));
        let result = run.finish(true).unwrap();
        assert_eq!(result.outcome, Outcome::Failed);
        assert!(
            result
                .failures
                .iter()
                .any(|f| f.starts_with("escape left processes behind")),
            "{:?}",
            result.failures
        );
        // Nothing is left, so the next run under the lock starts.
        let run = start(&state, LockKey::branch("x")).unwrap();
        assert_eq!(run.finish(true).unwrap().outcome, Outcome::Passed);
    }

    #[test]
    fn a_setsid_double_fork_survivor_is_caught_and_stopped() {
        survivor_case(false);
    }

    #[test]
    fn an_env_i_survivor_is_caught_and_stopped() {
        survivor_case(true);
    }

    #[test]
    fn what_ends_on_its_own_just_after_the_step_is_not_left_behind() {
        let repo = Repo::new();
        let state = State::open(&repo.checkout()).unwrap();
        let mut run = start(&state, LockKey::branch("x")).unwrap();
        // A background child that ends 600 ms after the step.
        let s = run
            .step("bg", &sh("/bin/sleep 0.6 & exit 0"), &repo.root, &[])
            .unwrap();
        assert_eq!(s.status, StepStatus::Passed, "{s:?}");
        // `git commit` starts `git maintenance run --auto --detach`, which leaves the step.
        fs::write(repo.root.join("c.txt"), "c\n").unwrap();
        let git_env: Vec<(String, String)> = [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_AUTHOR_NAME", "t"),
            ("GIT_AUTHOR_EMAIL", "t@atelier.invalid"),
            ("GIT_COMMITTER_NAME", "t"),
            ("GIT_COMMITTER_EMAIL", "t@atelier.invalid"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
        let s = run
            .step(
                "commit",
                &sh("git add c.txt && git -c commit.gpgsign=false commit -q -m c"),
                &repo.root,
                &git_env,
            )
            .unwrap();
        assert_eq!(s.status, StepStatus::Passed, "{s:?}");
        assert_eq!(run.finish(true).unwrap().outcome, Outcome::Passed);
    }

    #[test]
    fn a_marked_process_outside_the_jobs_is_reported_by_the_marker_scan_and_never_signalled() {
        let repo = Repo::new();
        let state = State::open(&repo.checkout()).unwrap();
        let mut run = start(&state, LockKey::branch("x")).unwrap();
        // A sleeper in its own session, outside every job's coalition, that carries this run's
        // marker: the coalition scan does not see it, the marker scan does. It is this test
        // program: macOS shows no other process the environment of Apple's own programs.
        let decoy_file = repo.tmp.path().join("decoy.pid");
        let argv = probe_argv();
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let status = Command::new("/bin/sh")
            .args([
                "-c",
                &daemonize_exec(&decoy_file.to_string_lossy(), false, &argv),
            ])
            .env(PROBE, "sleep")
            .env("ATELIER_RUN", run.id())
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let decoy = read_pid(&decoy_file);
        let step = run.step("t", &sh("true"), &repo.root, &[]).unwrap();
        assert_eq!(step.status, StepStatus::Passed, "{step:?}");
        let result = run.finish(true).unwrap();
        assert_eq!(result.outcome, Outcome::Failed, "{:?}", result.failures);
        assert!(
            result
                .survivors
                .iter()
                .any(|s| s.starts_with(&format!("{decoy} "))),
            "{:?}",
            result.survivors
        );
        assert!(alive(decoy), "a marked process is named, not signalled");
        // While it lives, the next run under the lock refuses, naming it.
        let e = start(&state, LockKey::branch("x")).unwrap_err();
        assert!(e.message.contains(&format!("{decoy} ")), "{}", e.message);
        assert!(alive(decoy));
        kill_and_wait(decoy);
        start(&state, LockKey::branch("x"))
            .unwrap()
            .finish(true)
            .unwrap();
    }

    #[test]
    fn an_outer_run_stops_what_a_killed_nested_atelier_left() {
        let repo = Repo::new();
        let state = State::open(&repo.checkout()).unwrap();
        let pidfile = repo.tmp.path().join("sleeper.pid");
        let inner_file = repo.tmp.path().join("inner");
        let mut run = start(&state, LockKey::Gate).unwrap();
        let outer = run.id().to_owned();
        let step = run
            .step(
                "nested",
                &probe_argv(),
                &repo.root,
                &[
                    (PROBE.into(), "nested-killed".into()),
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
        // The inner atelier was killed by its own step.
        assert_eq!(step.signal, Some(9), "{step:?}");
        let sleeper = read_pid(&pidfile);
        assert!(alive(sleeper));
        let inner = fs::read_to_string(&inner_file).unwrap();
        let inner_id = inner.split(' ').next().unwrap().to_owned();
        let info = RunInfo::read(&state.runs_dir().join(&inner_id))
            .unwrap()
            .unwrap();
        assert!(info.runs.ends_with(&[outer.clone(), inner_id.clone()]));
        let inner_label = format!("atelier.{inner_id}.1");
        assert!(label_loaded(&inner_label), "the inner job is still loaded");
        // The inner step's job lives on in its own coalition, listed in the outer run's file.
        let lines = coalition::read_run_file(run.dir()).unwrap().unwrap();
        assert!(lines.iter().any(|l| l.label == inner_label), "{lines:?}");
        // While it lives, the inner lock refuses, naming it, and signals nothing.
        let e = start(&state, LockKey::branch("inner")).unwrap_err();
        assert_eq!(e.exit, Exit::Refused);
        assert!(e.message.contains(&format!("{sleeper} ")), "{}", e.message);
        assert!(alive(sleeper));

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
        assert!(!alive(sleeper), "the outer run stopped it");
        // The inner lock is free again, and the job the dead atelier left loaded is removed.
        start(&state, LockKey::branch("inner"))
            .unwrap()
            .finish(true)
            .unwrap();
        assert!(!label_loaded(&inner_label));
        start(&state, LockKey::Gate).unwrap().finish(true).unwrap();
    }

    #[test]
    fn an_outer_run_stops_what_a_killed_nested_atelier_on_another_state_left() {
        let repo = Repo::new();
        let state = State::open(&repo.checkout()).unwrap();
        let other = repo.tmp.path().join("other-state");
        let pidfile = repo.tmp.path().join("sleeper.pid");
        let inner_file = repo.tmp.path().join("inner");
        let mut run = start(&state, LockKey::Gate).unwrap();
        let step = run
            .step(
                "nested",
                &probe_argv(),
                &repo.root,
                &[
                    (PROBE.into(), "nested-killed".into()),
                    (
                        "ATELIER_TEST_STATE".into(),
                        other.to_string_lossy().into_owned(),
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
        assert_eq!(step.signal, Some(9), "{step:?}");
        let sleeper = read_pid(&pidfile);
        assert!(alive(sleeper));
        let inner = fs::read_to_string(&inner_file).unwrap();
        let inner_id = inner.split(' ').next().unwrap().to_owned();
        let inner_label = format!("atelier.{inner_id}.1");
        // The inner run is in the other state, and its job is listed in the outer run's file.
        assert!(other.join("runs").join(&inner_id).is_dir());
        assert!(!state.runs_dir().join(&inner_id).exists());
        let lines = coalition::read_run_file(run.dir()).unwrap().unwrap();
        assert!(lines.iter().any(|l| l.label == inner_label), "{lines:?}");

        let result = run.finish(true).unwrap();
        assert_eq!(result.outcome, Outcome::Failed, "{result:?}");
        assert!(
            result
                .survivors
                .iter()
                .any(|s| s.starts_with(&format!("{sleeper} "))),
            "{:?}",
            result.survivors
        );
        assert!(!alive(sleeper), "the outer run stopped it");
        // The job the dead inner atelier left loaded is removed by the outer run's end, though
        // nobody takes the other state's lock again.
        wait_for(
            "the inner job to be removed",
            Duration::from_secs(10),
            || {
                launchd::sweep(&|_| false);
                !label_loaded(&inner_label)
            },
        );
        start(&state, LockKey::Gate).unwrap().finish(true).unwrap();
    }

    #[test]
    fn a_shim_launchd_cannot_spawn_is_reported_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let out = fs::canonicalize(dir.path()).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(&probe_argv()[1..])
            .env(PROBE, "missing-shim")
            .env("ATELIER_TEST_OUT", &out)
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let text = fs::read_to_string(out.join("result")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        let missing = out.join("no-such-atelier");
        for (i, what) in [
            (0, "unavailable: "),
            (2, "launchd jobs cannot be started: "),
        ] {
            let (secs, msg) = lines[i].split_once(' ').unwrap();
            let secs: f64 = secs.parse().unwrap();
            assert!(secs < 2.5, "{text}");
            assert!(msg.contains(what), "{text}");
            assert!(
                msg.contains(&format!(
                    "launchd could not start the shim: {}: No such file",
                    missing.display()
                )),
                "{text}"
            );
        }
        assert_eq!(lines[1], "loaded false", "{text}");
    }

    #[test]
    fn jobs_an_atelier_left_loaded_when_it_died_are_removed() {
        let fake_run = atelier::runs::new_run_id().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(&probe_argv()[1..])
            .env(PROBE, "die-after-bootstrap")
            .env("ATELIER_TEST_RUN", &fake_run)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let labels = [
            format!("atelier.probe.{}.0a1b", child.id()),
            format!("atelier.{fake_run}.1"),
            format!("atelier.{fake_run}.2"),
        ];
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(9));
        // All three are loaded, and every folder names the dead child as owner.
        for l in &labels {
            assert!(label_loaded(l), "{l}");
            assert!(launchd::job_dir(l).join("owner").is_file(), "{l}");
        }
        // Without a folder, the probe job is known by the pid in its label, the step job by
        // its run, which `dead_run` says is dead.
        fs::remove_dir_all(launchd::job_dir(&labels[0])).unwrap();
        fs::remove_dir_all(launchd::job_dir(&labels[1])).unwrap();
        wait_for("the jobs to be removed", Duration::from_secs(15), || {
            launchd::sweep(&|id| id == fake_run.as_str());
            labels.iter().all(|l| !label_loaded(l))
        });
        for l in &labels {
            assert!(!launchd::job_dir(l).exists(), "{l}");
        }
    }

    #[test]
    fn a_member_that_refuses_the_signal_is_reported_at_once() {
        let repo = Repo::new();
        let state = State::open(&repo.checkout()).unwrap();
        // `kill` answers EPERM for the pids in this file, as for a process `sudo` started.
        let deny = std::env::temp_dir().join(format!("atelier-deny-{}", std::process::id()));
        fs::write(&deny, "").unwrap();
        coalition::deny_signals_listed_in(&deny);
        let pidfile = repo.tmp.path().join("sleeper.pid");
        let mut run = start(&state, LockKey::branch("x")).unwrap();
        let t = Instant::now();
        let step = run
            .step(
                "sudo",
                &sh(&format!(
                    "{} && cat '{}' >> '{}'",
                    daemonize_cmd(&pidfile.to_string_lossy(), false),
                    pidfile.display(),
                    deny.display()
                )),
                &repo.root,
                &[],
            )
            .unwrap();
        let took = t.elapsed();
        let sleeper = read_pid(&pidfile);
        let user = String::from_utf8(
            Command::new("/usr/bin/id")
                .arg("-un")
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_owned();
        let want = format!("cannot stop {sleeper} (owned by {user})");
        assert_eq!(step.status, StepStatus::LeftProcesses, "{step:?}");
        assert_eq!(step.unstoppable, std::slice::from_ref(&want), "{step:?}");
        assert!(took < Duration::from_secs(5), "no retry rounds: {took:?}");
        assert!(alive(sleeper));
        let t = Instant::now();
        let result = run.finish(true).unwrap();
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        assert_eq!(result.outcome, Outcome::Failed);
        assert!(
            result.failures.contains(&format!("sudo: {want}")),
            "{:?}",
            result.failures
        );
        assert!(result.failures.contains(&want), "{:?}", result.failures);
        fs::write(&deny, "").unwrap();
        kill_and_wait(sleeper);
        let _ = fs::remove_file(&deny);
    }

    /// A run in a folder under `~/Documents`, which macOS protects (TCC). Not run by default: a
    /// launchd job is its own responsible process, so the first access from each new build of
    /// atelier (a new code signature) asks the owner in a dialog, which a headless run cannot
    /// answer and which would appear on the owner's screen during `scripts/check.sh`. Run it by
    /// hand at a Mac, once per release: `cargo test --test launchd -- --ignored documents`.
    #[test]
    #[ignore = "may show a macOS privacy dialog; run by hand"]
    fn a_run_in_a_folder_under_documents_works() {
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let docs = PathBuf::from(home).join("Documents");
        if !docs.is_dir() {
            eprintln!("skipped: no ~/Documents");
            return;
        }
        let dir = tempfile::tempdir_in(&docs).unwrap();
        let state = State::open_at(dir.path().join("state")).unwrap();
        let mut run = start(&state, LockKey::branch("x")).unwrap();
        fs::write(dir.path().join("in.txt"), "hello\n").unwrap();
        let s = run
            .step("read", &sh("cat in.txt"), dir.path(), &[])
            .unwrap();
        assert_eq!(s.status, StepStatus::Passed, "{s:?}");
        assert_eq!(
            fs::read_to_string(run.dir().join("read.log")).unwrap(),
            "hello\n"
        );
        assert_eq!(run.finish(true).unwrap().outcome, Outcome::Passed);
    }

    #[test]
    fn a_job_never_runs_its_program_unless_released() {
        steps_use_atelier();
        let dir = tempfile::tempdir().unwrap();
        let dir = fs::canonicalize(dir.path()).unwrap();
        let marker = dir.join("ran");
        for release in [false, true] {
            let spec = launchd::Spec {
                argv: vec!["/usr/bin/touch".into(), marker.clone().into()],
                cwd: dir.clone(),
                env: Vec::new(),
                stdout: dir.join("out"),
                stderr: dir.join("err"),
            };
            let label = format!("atelier.test.{}.{release}", std::process::id());
            let mut job = launchd::launch(&label, &spec, &mut || false).unwrap();
            let job_dir = launchd::job_dir(&label);
            assert_eq!(job.dir(), job_dir);
            // The intent record names this process.
            let owner = fs::read_to_string(job_dir.join("owner")).unwrap();
            assert!(
                owner.starts_with(&format!("{} ", std::process::id())),
                "{owner}"
            );
            assert!(job.coalition.id > 0);
            assert_eq!(atelier::procs::start_time(job.pid), job.start);
            if release {
                assert!(job.release());
            } else {
                job.abandon();
            }
            assert!(job.wait(&mut || false).unwrap());
            let (exit, _) = job.status();
            assert_eq!(exit, Some(if release { 0 } else { 125 }));
            assert_eq!(marker.exists(), release);
            assert_eq!(job.bootout(), None);
            assert!(!label_loaded(&label));
            assert!(!job_dir.exists());
        }
    }

    #[test]
    fn a_step_in_a_missing_folder_does_not_start() {
        let repo = Repo::new();
        let state = State::open(&repo.checkout()).unwrap();
        let mut run = start(&state, LockKey::branch("x")).unwrap();
        let s = run
            .step("lost", &sh("true"), &repo.root.join("nope"), &[])
            .unwrap();
        assert_eq!(s.status, StepStatus::NotStarted, "{s:?}");
        let log = fs::read_to_string(run.dir().join("lost.log")).unwrap();
        assert!(log.contains("nope"), "{log}");
        assert_eq!(run.finish(true).unwrap().outcome, Outcome::Failed);
    }

    #[test]
    fn the_probe_runs_once_per_macos_version_and_is_recorded() {
        let repo = Repo::new();
        let state = State::open(&repo.checkout()).unwrap();
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let record = state.root().join(format!("probes/coalition-{uid}.toml"));
        start(&state, LockKey::branch("x"))
            .unwrap()
            .finish(true)
            .unwrap();
        let text = fs::read_to_string(&record).unwrap();
        assert!(
            text.contains(&format!("macos = \"{}\"", coalition::macos_version())),
            "{text}"
        );
        // Another version's pass does not count: the probe runs again and records this one.
        fs::write(
            &record,
            "macos = \"10.0 (old)\"\npassed = \"x\"\natelier_version = \"0\"\n",
        )
        .unwrap();
        start(&state, LockKey::branch("x"))
            .unwrap()
            .finish(true)
            .unwrap();
        assert!(
            fs::read_to_string(&record)
                .unwrap()
                .contains(&coalition::macos_version())
        );
    }

    #[test]
    fn a_trivial_step_costs_tens_of_milliseconds() {
        let repo = Repo::new();
        let state = State::open(&repo.checkout()).unwrap();
        let mut run = start(&state, LockKey::branch("x")).unwrap();
        let n = 5;
        let t = Instant::now();
        for _ in 0..n {
            run.step("t", &["/usr/bin/true".into()], &repo.root, &[])
                .unwrap();
        }
        let each = t.elapsed() / n;
        eprintln!("launchd step: {} ms each", each.as_millis());
        assert!(each < Duration::from_secs(2), "{each:?}");
        run.finish(true).unwrap();
    }
}
