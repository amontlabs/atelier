//! End to end against a real Linux machine over real ssh, opt-in:
//!
//! ```sh
//! ATELIER_E2E_SSH=10.77.77.2 cargo test --test remote_e2e -- --ignored --nocapture
//! ```
//!
//! It uses a throwaway project here, and on the machine only `~/atelier-exp/` (the project's
//! checkout), `~/.local/share/atelier/<version>/` (the pinned atelier, built there by
//! `machines init`) and `~/.local/state/atelier/` (the machine lock). It removes none of them:
//! whoever runs it cleans the machine afterwards. Covered: `machines init` building from source
//! (its plan; replacing an install of other source, or a broken one, only with `--yes`, after
//! the exchange probe compiled there with `rustc`, swapped in with `renameat2(RENAME_EXCHANGE)`
//! and one replaced folder kept), `machines`, a passing and a failing `test`, the remote tree
//! recorded, a connection killed mid-run (`ssh -tt`, the terminal hang-up and the watcher):
//! `connection lost`, exit 3, the remote group stopped; and an ignored file in the way of the
//! checkout: not ready, the file and the checkout unchanged.

mod common;

use std::fs;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::{Repo, VERSION, atelier_bin, code, stderr, stdout};

fn both(out: &Output) -> String {
    format!("{}{}", stdout(out), stderr(out))
}

fn ssh(host: &str, script: &str) -> Output {
    Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            host,
            script,
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
#[ignore = "needs ATELIER_E2E_SSH=<host> of a Linux x86_64 machine"]
fn a_real_linux_machine() {
    let Ok(host) = std::env::var("ATELIER_E2E_SSH") else {
        eprintln!("ATELIER_E2E_SSH is not set: skipped");
        return;
    };
    let repo = Repo::new();
    repo.install_self(VERSION);
    repo.write(
        ".atelier/atelier.toml",
        &format!(
            "atelier = \"{VERSION}\"\n\n[platforms.linux-x86_64]\nprepare = [\"sh\", \"-c\", \"echo prepared on $(uname -s)\"]\nrun = [\"sh\", \"-c\", \"uname -sm | grep -q 'Linux x86_64'\"]\n"
        ),
    );
    repo.write_rules();
    repo.write("sub/keep", "x\n");
    repo.commit_all("settings");
    let machines = format!(
        r#"
[machines.linux-box]
ssh = "{host}"
platform = "linux-x86_64"
priority = 10
connect_timeout_seconds = 5
path = ["~/.local/bin", "~/.cargo/bin", "/usr/local/bin", "/usr/bin", "/bin"]
checkouts = "~/atelier-exp/checkouts"
"#
    );
    let mf = repo.home.join(".config/atelier/machines.toml");
    fs::create_dir_all(mf.parent().unwrap()).unwrap();
    fs::write(&mf, machines).unwrap();
    let atelier = |args: &[&str]| -> Command {
        let mut c = Command::new(atelier_bin());
        c.args(args)
            .current_dir(&repo.root)
            .env("HOME", &repo.home)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("ATELIER_DISPATCHED")
            .env_remove("ATELIER_RUN")
            .env_remove("ATELIER_RUNS")
            .env_remove("ATELIER_SSH");
        c
    };

    let t = Instant::now();
    let out = atelier(&["machines", "init", "linux-box"])
        .output()
        .unwrap();
    eprintln!("machines init ({:?}):\n{}", t.elapsed(), both(&out));
    assert_eq!(code(&out), 0);
    assert!(stdout(&out).starts_with("linux-box: plan\n"));

    // An install of other source there: replaced only with --yes, the old folder kept.
    let bin = format!("~/.local/share/atelier/{VERSION}/bin/atelier");
    let o = ssh(
        &host,
        &format!(
            "rm {bin} && printf '#!/bin/sh\\necho {VERSION} 0000000000000000\\n' > {bin} && chmod +x {bin}"
        ),
    );
    assert!(o.status.success(), "{}", both(&o));
    let out = atelier(&["machines", "init", "linux-box"])
        .output()
        .unwrap();
    eprintln!("machines init over another install:\n{}", both(&out));
    assert_eq!(code(&out), 4);
    assert!(stderr(&out).contains("--yes"));
    let t = Instant::now();
    let out = atelier(&["machines", "init", "linux-box", "--yes"])
        .output()
        .unwrap();
    eprintln!("machines init --yes ({:?}):\n{}", t.elapsed(), both(&out));
    assert_eq!(code(&out), 0);
    assert!(stdout(&out).contains("the replaced install is kept at "));
    let o = ssh(
        &host,
        &format!("cat ~/.local/share/atelier/.{VERSION}.replaced-*/bin/atelier"),
    );
    assert!(stdout(&o).contains("0000000000000000"), "{}", both(&o));

    // A broken install (the version folder without bin/atelier): named in the plan, replaced
    // only with --yes, after the exchange probe compiled there; one replaced folder is left.
    let o = ssh(&host, &format!("rm {bin}"));
    assert!(o.status.success(), "{}", both(&o));
    let out = atelier(&["machines", "init", "linux-box"])
        .output()
        .unwrap();
    eprintln!("machines init over a broken install:\n{}", both(&out));
    assert_eq!(code(&out), 4);
    assert!(stdout(&out).contains("replace a broken install"));
    let t = Instant::now();
    let out = atelier(&["machines", "init", "linux-box", "--yes"])
        .output()
        .unwrap();
    eprintln!(
        "machines init --yes over a broken install ({:?}):\n{}",
        t.elapsed(),
        both(&out)
    );
    assert_eq!(code(&out), 0);
    let o = ssh(
        &host,
        &format!("ls -d ~/.local/share/atelier/.{VERSION}.replaced-* | wc -l"),
    );
    assert_eq!(stdout(&o).trim(), "1", "{}", both(&o));

    let out = atelier(&["machines"]).output().unwrap();
    eprintln!("machines:\n{}", both(&out));
    assert!(stdout(&out).contains("reachable, ready (prerequisites checked at run time), free"));

    let t = Instant::now();
    let out = atelier(&["test", "linux-x86_64"]).output().unwrap();
    eprintln!("full run ({:?}):\n{}", t.elapsed(), both(&out));
    assert_eq!(code(&out), 0);
    assert_eq!(stdout(&out).trim(), "linux-x86_64: PASS on linux-box");
    let o = ssh(
        &host,
        "cd ~/atelier-exp/checkouts/* && git rev-parse HEAD^{tree} && grep -h '^tree = ' .git/atelier/runs/*/result",
    );
    eprintln!("remote tree and recorded trees:\n{}", stdout(&o));
    let text = stdout(&o);
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines.len() >= 2 && lines[1..].iter().all(|l| l.contains(lines[0])));

    let mut c = atelier(&[
        "test",
        "linux-x86_64",
        "--",
        "sh",
        "-c",
        "test \"$(basename \"$PWD\")\" = sub && echo in-sub",
    ]);
    let out = c.current_dir(repo.root.join("sub")).output().unwrap();
    eprintln!("narrow run in sub/:\n{}", both(&out));
    assert_eq!(code(&out), 0);

    let out = atelier(&[
        "test",
        "linux-x86_64",
        "--",
        "sh",
        "-c",
        "echo failing on purpose; exit 7",
    ])
    .output()
    .unwrap();
    eprintln!("failing run:\n{}", both(&out));
    assert_eq!(code(&out), 1);
    assert_eq!(stdout(&out).trim(), "linux-x86_64: FAIL on linux-box");

    // A connection killed mid-run.
    let _ = ssh(&host, "rm -f ~/atelier-exp/step.pid");
    let child = atelier(&[
        "test",
        "linux-x86_64",
        "--",
        "sh",
        "-c",
        "echo $$ > \"$HOME/atelier-exp/step.pid\"; exec sleep 300",
    ])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    let until = Instant::now() + Duration::from_secs(60);
    let step = loop {
        let o = ssh(&host, "cat ~/atelier-exp/step.pid 2>/dev/null");
        if let Ok(p) = stdout(&o).trim().parse::<i32>() {
            break p;
        }
        assert!(Instant::now() < until, "the remote step never started");
        std::thread::sleep(Duration::from_millis(300));
    };
    eprintln!("remote step pid {step}");
    // The session's `ssh -tt`: the child of this test's atelier whose command says so.
    let ps = Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
        .unwrap();
    let ssh_pid = stdout(&ps)
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let pid: i32 = w.next()?.parse().ok()?;
            let ppid: u32 = w.next()?.parse().ok()?;
            let cmd: Vec<&str> = w.collect();
            (ppid == child.id() && cmd.first() == Some(&"ssh") && cmd.get(1) == Some(&"-tt"))
                .then_some(pid)
        })
        .next()
        .expect("the session's ssh -tt");
    // SAFETY: the ssh client this test's atelier started, found by parent pid and command.
    unsafe { libc::kill(ssh_pid, libc::SIGKILL) };
    let t = Instant::now();
    let out = child.wait_with_output().unwrap();
    eprintln!("killed connection ({:?}):\n{}", t.elapsed(), both(&out));
    assert_eq!(code(&out), 3);
    assert_eq!(
        stdout(&out).trim(),
        "linux-x86_64: connection lost on linux-box"
    );

    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let o = ssh(
            &host,
            &format!("kill -0 {step} 2>/dev/null && echo alive || echo gone"),
        );
        if stdout(&o).trim() == "gone" {
            break;
        }
        assert!(Instant::now() < until, "remote step {step} still alive");
        std::thread::sleep(Duration::from_millis(500));
    }
    let o = ssh(
        &host,
        "cd ~/atelier-exp/checkouts/*/.git/atelier/runs && for r in *; do echo \"== $r\"; grep -E '^(outcome|signal|failures) ' \"$r/result\" 2>/dev/null; done",
    );
    eprintln!("remote results:\n{}", stdout(&o));
    assert!(stdout(&o).contains("failures = [\"connection lost\"]"));

    // An ignored file on the machine where the next commit tracks one: not ready, the file and
    // the checkout left as they were, the same answer twice.
    repo.write(".gitignore", ".env\n");
    repo.commit_all("ignore .env");
    let out = atelier(&["test", "linux-x86_64"]).output().unwrap();
    assert_eq!(code(&out), 0, "{}", both(&out));
    let o = ssh(
        &host,
        "cd ~/atelier-exp/checkouts/* && echo 'machine secret' > .env && git rev-parse HEAD",
    );
    let head = stdout(&o).trim().to_owned();
    repo.write(".env", "from the commit\n");
    let g = Command::new("git")
        .args(["add", "-f", ".env"])
        .current_dir(&repo.root)
        .output()
        .unwrap();
    assert!(g.status.success());
    repo.commit_all("track .env");
    let mut lines = Vec::new();
    for _ in 0..2 {
        let out = atelier(&["test", "linux-x86_64"]).output().unwrap();
        eprintln!("ignored file in the way:\n{}", both(&out));
        assert_eq!(code(&out), 3);
        let line = stdout(&out).trim().to_owned();
        assert!(
            line.starts_with(
                "linux-x86_64: no machine available (linux-box not ready: checking out "
            ) && line.ends_with("move them away: .env)"),
            "{line}"
        );
        lines.push(line);
        let o = ssh(
            &host,
            "cd ~/atelier-exp/checkouts/* && cat .env && git rev-parse HEAD",
        );
        assert_eq!(stdout(&o), format!("machine secret\n{head}\n"));
    }
    assert_eq!(lines[0], lines[1]);
}
