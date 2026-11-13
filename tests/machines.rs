//! `test`, `machines`, `machines init` and `remote-run`, with remote machines played by
//! `tests/fixtures/fake-ssh` (this computer has no ssh server for a real localhost session): it
//! runs the remote command here, as `sh -c` in that machine's own HOME with a fresh environment,
//! and relays its stdin, so that killing it gives the remote side end-of-input as a cut
//! connection does. The pinned atelier "installed" on each fake machine is the binary under
//! test. The real ssh path (`ssh -tt`, the terminal hang-up, `machines init` building from
//! source) is covered by `remote_e2e.rs` against a real machine.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{Repo, VERSION, atelier_bin, code, git, stderr, stdout};

fn here() -> String {
    atelier::platform::Platform::current().unwrap().to_string()
}

fn other_platform() -> &'static str {
    if here() == "linux-x86_64" {
        "macos-aarch64"
    } else {
        "linux-x86_64"
    }
}

fn fake_ssh() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-ssh")
}

struct World {
    repo: Repo,
    /// `FAKE_SSH_ROOT`: one folder per fake machine, its HOME.
    machines: PathBuf,
}

impl World {
    /// A project pinned to this version, with settings `extra` added, committed; machines:
    /// `down` (offline, priority 5), `box` (remote, priority 10), `here` (this computer, 100).
    fn new(extra: &str) -> World {
        let repo = Repo::new();
        repo.install_self(VERSION);
        let machines = repo.tmp.path().canonicalize().unwrap().join("machines");
        fs::create_dir_all(&machines).unwrap();
        let w = World { repo, machines };
        w.write_settings(extra);
        w.repo.write(".gitignore", "out/\n");
        w.repo.write("sub/keep", "x\n");
        w.repo.commit_all("settings");
        w.machines_file(None);
        w.add_machine("box");
        w
    }

    fn write_settings(&self, extra: &str) {
        self.repo.write(
            ".atelier/atelier.toml",
            &format!(
                "atelier = \"{VERSION}\"\n\n[platforms.{}]\nrun = [\"sh\", \"-c\", \"exit 0\"]\n{extra}",
                here()
            ),
        );
    }

    fn machines_file(&self, extra: Option<&str>) {
        let p = here();
        self.repo.write_home(
            ".config/atelier/machines.toml",
            &format!(
                r#"
[machines.down]
ssh = "down"
platform = "{p}"
priority = 5
connect_timeout_seconds = 1

[machines.box]
ssh = "box"
platform = "{p}"
priority = 10
path = ["/usr/bin", "/bin"]
checkouts = "~/checkouts"

[machines.here]
local = true
platform = "{p}"
gui = true
priority = 100
{}"#,
                extra.unwrap_or("")
            ),
        );
    }

    /// A fake machine with the binary under test installed as the pinned version.
    fn add_machine(&self, name: &str) -> PathBuf {
        let home = self.machines.join(name);
        let bin = home.join(format!(".local/share/atelier/{VERSION}/bin"));
        fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(atelier_bin(), bin.join("atelier")).unwrap();
        home
    }

    fn command(&self, dir: &Path, args: &[&str]) -> Command {
        let mut c = Command::new(atelier_bin());
        c.args(args)
            .current_dir(dir)
            .env("HOME", &self.repo.home)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("ATELIER_SSH", fake_ssh())
            .env("FAKE_SSH_ROOT", &self.machines)
            .env_remove("ATELIER_DISPATCHED")
            .env_remove("ATELIER_RUN")
            .env_remove("ATELIER_RUNS");
        c
    }

    fn run_in(&self, dir: &Path, args: &[&str]) -> Output {
        self.command(dir, args).output().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_in(&self.repo.root, args)
    }

    fn init_box(&self) {
        let out = self.run(&["machines", "init", "box"]);
        assert_eq!(code(&out), 0, "{}{}", stdout(&out), stderr(&out));
    }

    /// The project's checkout on `box`.
    fn box_checkout(&self) -> PathBuf {
        let dir = self.machines.join("box/checkouts");
        let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1);
        entries.pop().unwrap()
    }

    /// The results of the runs in a checkout's state folder, by run id.
    fn results(&self, checkout: &Path) -> Vec<atelier::runs::RunResult> {
        let runs = checkout.join(".git/atelier/runs");
        let mut out = Vec::new();
        for e in fs::read_dir(runs).into_iter().flatten() {
            if let Some(r) = atelier::runs::RunResult::read(&e.unwrap().path()).unwrap() {
                out.push(r);
            }
        }
        out.sort_by(|a, b| a.started.cmp(&b.started).then(a.id.cmp(&b.id)));
        out
    }
}

trait HomeWrite {
    fn write_home(&self, rel: &str, contents: &str);
}

impl HomeWrite for Repo {
    fn write_home(&self, rel: &str, contents: &str) {
        let p = self.home.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, contents).unwrap();
    }
}

fn both(out: &Output) -> String {
    format!("{}{}", stdout(out), stderr(out))
}

#[test]
fn a_machine_without_the_projects_checkout_is_skipped_and_named() {
    let w = World::new("");
    let out = w.run(&["test", &here()]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    let line = stdout(&out);
    assert!(
        line.starts_with(&format!(
            "{}: PASS on here (local) (down offline, box not ready: no checkout of this project at ~/checkouts/",
            here()
        )),
        "{line}"
    );
    assert!(line.contains(": run atelier machines init box)"), "{line}");
    // The local run is a run of this worktree, with no machine name.
    let results = w.results(&w.repo.root);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].machine, None);
    assert_eq!(results[0].platform.as_deref(), Some(here().as_str()));
    assert_eq!(results[0].kind, atelier::runs::RunKind::TestFull);
}

#[test]
fn machines_init_creates_the_checkout_and_finds_the_pinned_atelier() {
    let w = World::new("");
    let out = w.run(&["machines", "init", "box"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    assert!(
        stdout(&out).contains("box: checkout ~/checkouts/"),
        "{}",
        both(&out)
    );
    assert!(stdout(&out).contains("(created)"), "{}", both(&out));
    assert!(stdout(&out).contains("already installed"), "{}", both(&out));
    assert!(w.box_checkout().join(".git").is_dir());
    let again = w.run(&["machines", "init", "box"]);
    assert!(stdout(&again).contains("(exists)"), "{}", both(&again));

    assert_eq!(code(&w.run(&["machines", "init", "down"])), 3);
    assert_eq!(code(&w.run(&["machines", "init", "nope"])), 4);
    let local = w.run(&["machines", "init", "here"]);
    assert_eq!(code(&local), 0);
    assert!(stdout(&local).contains("this computer"));
}

#[test]
fn machines_lists_reachable_ready_and_busy() {
    let w = World::new("");
    w.init_box();
    let out = w.run(&["machines"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    let text = stdout(&out);
    let line = |name: &str| {
        text.lines()
            .find(|l| l.starts_with(&format!("{name} ")))
            .unwrap_or_else(|| panic!("{name} in {text}"))
            .to_owned()
    };
    assert!(line("down").contains("offline"), "{text}");
    assert!(line("box").contains("ssh box"), "{text}");
    assert!(line("box").ends_with("reachable, ready, free"), "{text}");
    assert!(
        line("here").contains("this computer, ready, free"),
        "{text}"
    );
    // Ordered by priority.
    let order: Vec<&str> = text
        .lines()
        .skip(1)
        .map(|l| l.split(' ').next().unwrap())
        .collect();
    assert_eq!(order, ["down", "box", "here"]);

    let _held = hold_machine_lock(&w.machines.join("box"));
    let out = w.run(&["machines"]);
    assert!(
        stdout(&out).contains("reachable, ready, busy (pid "),
        "{}",
        both(&out)
    );
}

#[test]
fn a_remote_run_passes_fails_and_records_where_it_ran() {
    let w = World::new("");
    w.init_box();
    let p = here();
    let out = w.run(&["test", &p, "--", "sh", "-c", "echo remote-output; exit 0"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    assert_eq!(
        stdout(&out).trim(),
        format!("{p}: PASS on box (down offline)")
    );

    // The remote checkout ran the pushed commit, and kept its own run.
    let head = git(&w.repo.root, &["rev-parse", "HEAD"]);
    let checkout = w.box_checkout();
    assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), head);
    let remote = w.results(&checkout);
    assert_eq!(remote.len(), 1);
    assert_eq!(remote[0].machine.as_deref(), Some("box"));
    assert_eq!(remote[0].commit.as_deref(), Some(head.as_str()));
    assert_eq!(remote[0].kind, atelier::runs::RunKind::TestNarrow);
    assert_eq!(remote[0].outcome, atelier::runs::Outcome::Passed);

    // This computer's run names the machine and holds the streamed log.
    let local = w.results(&w.repo.root);
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].machine.as_deref(), Some("box"));
    assert_eq!(local[0].outcome, atelier::runs::Outcome::Passed);
    let run_dir = w.repo.root.join(".git/atelier/runs").join(&local[0].id);
    let log = fs::read_to_string(run_dir.join("remote-box.log")).unwrap();
    assert!(log.contains("remote-output"), "{log}");
    assert_eq!(
        fs::read_to_string(run_dir.join("remote-runs"))
            .unwrap()
            .trim(),
        remote[0].id
    );

    let out = w.run(&["test", &p, "--", "sh", "-c", "exit 3"]);
    assert_eq!(code(&out), 1, "{}", both(&out));
    assert_eq!(
        stdout(&out).trim(),
        format!("{p}: FAIL on box (down offline)")
    );
    assert!(
        stderr(&out).contains("box: command failed"),
        "{}",
        both(&out)
    );
}

#[test]
fn the_command_runs_in_the_callers_folder_and_the_whole_run_without_one() {
    let other = other_platform();
    let w = World::new(&format!(
        r#"prepare = ["sh", "-c", "echo prepare >> out/order"]

[platforms.{other}]
run = "check"

[[check.prepare]]
name = "first"
run = ["sh", "-c", "rm -rf out && mkdir out && echo hook >> out/order"]
[[check.prepare]]
name = "elsewhere"
run = ["sh", "-c", "echo wrong >> out/order"]
platforms = ["{other}"]
"#
    ));
    // Replace the platform's run with one that checks what ran before it.
    let p = here();
    let text = fs::read_to_string(w.repo.root.join(".atelier/atelier.toml")).unwrap();
    fs::write(
        w.repo.root.join(".atelier/atelier.toml"),
        text.replace(
            "run = [\"sh\", \"-c\", \"exit 0\"]",
            "run = [\"sh\", \"-c\", \"test \\\"$(cat out/order | tr '\\\\n' ' ')\\\" = 'hook prepare '\"]",
        ),
    )
    .unwrap();
    w.repo.commit_all("run checks the order");
    w.init_box();

    let out = w.run(&["test", &p]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    let remote = w.results(&w.box_checkout());
    let names: Vec<&str> = remote[0].steps.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["prepare-first", "prepare", "run"]);
    assert_eq!(remote[0].kind, atelier::runs::RunKind::TestFull);

    // A narrow run: the same preparation, then the command, in the caller's folder.
    let out = w.run_in(
        &w.repo.root.join("sub"),
        &[
            "test",
            &p,
            "--",
            "sh",
            "-c",
            "test \"$(basename \"$PWD\")\" = sub && test -f keep",
        ],
    );
    assert_eq!(code(&out), 0, "{}", both(&out));
    let remote = w.results(&w.box_checkout());
    let names: Vec<&str> = remote[1].steps.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["prepare-first", "prepare", "command"]);
}

#[test]
fn a_step_that_changes_the_tree_fails_the_run() {
    let w = World::new("");
    w.init_box();
    let out = w.run(&["test", &here(), "--", "sh", "-c", "echo x > new-file"]);
    assert_eq!(code(&out), 1, "{}", both(&out));
    assert!(
        stderr(&out).contains("the run changed the tree"),
        "{}",
        both(&out)
    );
    // The file stays in the remote checkout and makes it not ready; nothing deletes it.
    let out = w.run(&["test", &here(), "--", "true"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    assert!(
        stdout(&out).contains("box not ready: the checkout ~/checkouts/"),
        "{}",
        both(&out)
    );
    assert!(stdout(&out).contains("?? new-file"), "{}", both(&out));
    assert!(
        stdout(&out).contains("PASS on here (local)"),
        "{}",
        both(&out)
    );
    assert!(w.box_checkout().join("new-file").is_file());
}

#[test]
fn uncommitted_changes_windows_and_unknown_platforms_are_refused() {
    let w = World::new("\n[platforms.windows-x86_64]\nrun = \"check\"\n");
    let p = here();
    w.repo.write("untracked", "x\n");
    let out = w.run(&["test", &p]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    assert!(
        stderr(&out).contains("uncommitted changes"),
        "{}",
        both(&out)
    );
    assert!(stderr(&out).contains("?? untracked"), "{}", both(&out));
    fs::remove_file(w.repo.root.join("untracked")).unwrap();

    let out = w.run(&["test", "windows-x86_64"]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    assert!(stderr(&out).contains("Windows machines are not supported"));
    let out = w.run(&["test", "--all"]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    let out = w.run(&["test", other_platform()]);
    assert_eq!(code(&out), 4, "{}", both(&out));
    assert!(stderr(&out).contains("is not a platform of this project"));
    let out = w.run(&["test", "darwin"]);
    assert_eq!(code(&out), 2, "{}", both(&out));
}

#[test]
fn no_machine_available_exits_3_listing_each_reason() {
    let p = here();
    let w = World::new("machines = [\"down\", \"box\", \"gone\"]\n");
    let out = w.run(&["test", &p, "--", "true"]);
    assert_eq!(code(&out), 3, "{}", both(&out));
    let err = stderr(&out);
    assert!(
        err.contains(&format!("{p}: no machine available:")),
        "{err}"
    );
    assert!(
        err.contains("  down: offline (ssh: connect to host down"),
        "{err}"
    );
    assert!(err.contains("  box: not ready: no checkout"), "{err}");
    assert!(err.contains("  gone: is not in machines.toml"), "{err}");
    assert!(
        stdout(&out).starts_with(&format!(
            "{p}: no machine available (down offline, box not ready"
        )),
        "{}",
        both(&out)
    );
    let results = w.results(&w.repo.root);
    assert_eq!(results[0].outcome, atelier::runs::Outcome::Failed);
    assert!(results[0].failures[0].starts_with("no machine available: "));
}

/// Holds `<home>/.local/state/atelier/machine.lock` in a child process until dropped.
struct HeldLock(Child);

impl Drop for HeldLock {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        let _ = self.0.wait();
    }
}

fn hold_machine_lock(home: &Path) -> HeldLock {
    let dir = home.join(".local/state/atelier");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("machine.holder"), "pid 1 since then: the test\n").unwrap();
    // `flock` is not on macOS by default; perl's is.
    let mut child = Command::new("/usr/bin/perl")
        .arg("-e")
        .arg("use Fcntl ':flock'; open(my $f, '>>', $ARGV[0]) or die; flock($f, LOCK_EX) or die; print \"held\\n\"; $| = 1; <STDIN>;")
        .arg(dir.join("machine.lock"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(child.stdout.as_mut().unwrap()),
        &mut line,
    )
    .unwrap();
    assert_eq!(line.trim(), "held");
    HeldLock(child)
}

#[test]
fn a_busy_machine_is_skipped_for_the_next_one() {
    let w = World::new("");
    w.init_box();
    let _held = hold_machine_lock(&w.machines.join("box"));
    let out = w.run(&["test", &here(), "--", "true"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    assert_eq!(
        stdout(&out).trim(),
        format!("{}: PASS on here (local) (down offline, box busy)", here())
    );
}

#[test]
fn when_only_busy_machines_are_left_it_waits_for_one() {
    let w = World::new("machines = [\"down\", \"box\"]\n");
    w.init_box();
    let held = hold_machine_lock(&w.machines.join("box"));
    let start = Instant::now();
    let child = w
        .command(&w.repo.root, &["test", &here(), "--", "true"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    drop(held);
    let out = child.wait_with_output().unwrap();
    assert_eq!(code(&out), 0, "{}", both(&out));
    assert!(
        stderr(&out).contains("waiting for a busy machine to free up: box"),
        "{}",
        both(&out)
    );
    assert_eq!(
        stdout(&out).trim(),
        format!("{}: PASS on box (down offline)", here())
    );
    assert!(start.elapsed() >= Duration::from_secs(2));
}

#[test]
fn a_local_machine_holds_the_machine_lock_while_it_runs() {
    let w = World::new("machines = [\"here\"]\n");
    let _held = hold_machine_lock(&w.repo.home);
    let mut child = w
        .command(&w.repo.root, &["test", &here(), "--", "true"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Waiting: the only machine is busy.
    let mut err = std::io::BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    std::io::BufRead::read_line(&mut err, &mut line).unwrap();
    assert!(
        line.contains("waiting for a busy machine to free up: here"),
        "{line}"
    );
    assert!(child.try_wait().unwrap().is_none());
    // SAFETY: our own child; TERM is a stop signal for atelier.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{:?} {}",
        out.status,
        both(&out)
    );
    assert!(stdout(&out).contains("INTERRUPTED"), "{}", both(&out));
    let results = w.results(&w.repo.root);
    assert_eq!(results[0].outcome, atelier::runs::Outcome::Interrupted);
}

fn read_pid(path: &Path, within: Duration) -> i32 {
    let until = Instant::now() + within;
    loop {
        if let Ok(t) = fs::read_to_string(path)
            && let Ok(p) = t.trim().parse()
        {
            return p;
        }
        assert!(Instant::now() < until, "no pid in {}", path.display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: existence check only.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn a_connection_killed_mid_run_is_connection_lost_and_the_remote_group_stops() {
    let w = World::new("");
    w.init_box();
    let pid_file = w.machines.join("box/step.pid");
    let child = w
        .command(
            &w.repo.root,
            &[
                "test",
                &here(),
                "--",
                "sh",
                "-c",
                "echo $$ > \"$HOME/step.pid\"; exec sleep 300",
            ],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let step = read_pid(&pid_file, Duration::from_secs(30));
    assert!(alive(step));
    // The session's ssh, as the fake records it: kill it as a cut connection would end it.
    let ssh = read_pid(&w.machines.join("box.tty-pids"), Duration::from_secs(5));
    // SAFETY: the pid of the stand-in ssh this test's atelier started, read from its record.
    unsafe { libc::kill(ssh, libc::SIGKILL) };
    let out = child.wait_with_output().unwrap();
    assert_eq!(code(&out), 3, "{}", both(&out));
    assert_eq!(
        stdout(&out).trim(),
        format!("{}: connection lost on box (down offline)", here())
    );
    assert!(
        stderr(&out).contains("box: connection lost"),
        "{}",
        both(&out)
    );

    let until = Instant::now() + Duration::from_secs(15);
    while alive(step) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!alive(step), "the remote step {step} still runs");
    // The remote side wrote its own account.
    let until = Instant::now() + Duration::from_secs(15);
    let remote = loop {
        let r = w.results(&w.box_checkout());
        if !r.is_empty() || Instant::now() > until {
            break r;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(remote.len(), 1);
    assert_eq!(remote[0].outcome, atelier::runs::Outcome::Interrupted);
    assert_eq!(remote[0].signal.as_deref(), Some("SIGHUP"));
    assert!(
        remote[0].failures.iter().any(|f| f == "connection lost"),
        "{:?}",
        remote[0]
    );
    let local = w.results(&w.repo.root);
    assert_eq!(local[0].outcome, atelier::runs::Outcome::Failed);
    assert!(
        local[0]
            .failures
            .iter()
            .any(|f| f.starts_with("box: connection lost"))
    );
}

#[test]
fn a_stale_remote_atelier_is_not_ready() {
    let w = World::new("");
    w.init_box();
    // Another program at the pinned version's place: an identity that differs.
    let bin = w
        .machines
        .join(format!("box/.local/share/atelier/{VERSION}/bin/atelier"));
    fs::remove_file(&bin).unwrap();
    fs::write(
        &bin,
        "#!/bin/sh\n[ \"$1 $2\" = 'remote-run identity' ] && { echo '0.0.0 0000000000000000'; exit 0; }\nprintf '%s\\n' \"$(echo \"$@\" | sed -n 's/.*--nonce \\([0-9a-f]*\\).*/atelier-remote-\\1: identity 0.0.0 0000000000000000/p')\"\nprintf '%s\\n' \"$(echo \"$@\" | sed -n 's/.*--nonce \\([0-9a-f]*\\).*/atelier-remote-\\1: ready/p')\"\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let out = w.run(&["test", &here(), "--", "true"]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    assert!(
        stdout(&out).contains("box not ready: atelier there is 0.0.0 0000000000000000"),
        "{}",
        both(&out)
    );
}
