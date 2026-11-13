//! This computer's side of a run on another machine (DESIGN §2.5, r4 §4.4, B3).
//!
//! Trying a remote machine takes three ssh connections, then a fourth to read the verdict:
//!
//! 1. **Probe** (plain ssh, `ConnectTimeout`): no answer is `offline`. The pinned atelier must be
//!    installed there, built from this binary's own source ([`crate::source::identity`]); its
//!    `remote-run probe` checks the platform from `uname`, the project's checkout, the machine
//!    preflight with this checkout's settings (sent on stdin), and the machine lock (`busy`).
//! 2. **Push** the commit over ssh into the machine's checkout for the project, to
//!    `refs/atelier/test`. Nothing goes to GitHub.
//! 3. **Session**: `ssh -tt` runs `remote-run run`, with `PATH` set to exactly the machine's
//!    `path`. It preflights again from the pushed commit's settings, takes the machine lock,
//!    requires a clean checkout (unknown files are listed, never deleted), checks the commit
//!    out, and runs the job. Its input is the terminal: when the connection goes, the session
//!    gets a hang-up and the remote atelier also sees end-of-input; either way it stops its
//!    group and writes `connection lost`. ssh runs with `ServerAliveInterval=5` and
//!    `ServerAliveCountMax=3`, so a connection that dies silently is noticed here in about 15 s.
//!    The terminal carries the logs and the protocol lines, each marked with a per-session
//!    nonce so that a step's output cannot forge one.
//! 4. **Verdict**: each run's `result` file, read with a second ssh. A session that ended
//!    without its `done` line is `connection lost` (exit 3), never a pass.
//!
//! Remote commands are run as `exec /bin/sh -c '<script>'`, so any login shell works.
//! `ATELIER_SSH` replaces the `ssh` program (the tests use a stand-in that runs the command on
//! this computer).

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread::{JoinHandle, sleep};
use std::time::{Duration, Instant};

use crate::dispatch::VERSION;
use crate::git::{self, Checkout};
use crate::jobs::Job;
use crate::machines::{Attempt, Machine};
use crate::runs::RunResult;
use crate::signals;
use crate::source;

/// Replaces the `ssh` program.
pub const SSH_VAR: &str = "ATELIER_SSH";

/// The ref a pushed commit lands on in the machine's checkout.
pub const PUSH_REF: &str = "refs/atelier/test";

/// How long the verdict is waited for after a session ended without `done`: the remote side
/// may still be stopping its group (TERM, a grace period, KILL) and writing its result.
const RESULT_WAIT: Duration = Duration::from_secs(20);

/// Protocol lines start with this, then the session's nonce and `: `.
pub const PROTOCOL: &str = "atelier-remote-";

/// The protocol prefix of one session.
pub fn tag(nonce: &str) -> String {
    format!("{PROTOCOL}{nonce}: ")
}

/// A shell word that is exactly `s`.
pub fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A shell word for a path that may start with `~/`, expanded with the machine's `$HOME`.
pub fn shell_path(p: &str) -> String {
    if p == "~" {
        "\"$HOME\"".to_owned()
    } else if let Some(rest) = p.strip_prefix("~/") {
        format!("\"$HOME\"/{}", sq(rest))
    } else {
        sq(p)
    }
}

/// The project's name on other machines: the main checkout's folder name and a hash of its
/// git folder, so two projects with one name stay apart.
pub fn project_id(checkout: &Checkout) -> String {
    let common = git::canonical(&checkout.common_dir);
    let main = if common.file_name().is_some_and(|n| n == ".git") {
        common
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(common.clone())
    } else {
        common.clone()
    };
    let name: String = main
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in common.to_string_lossy().bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let name = name.trim_start_matches('.');
    format!(
        "{}-{:08x}",
        if name.is_empty() { "project" } else { name },
        h >> 32
    )
}

/// A remote machine as this computer reaches it.
#[derive(Debug, Clone)]
pub struct Remote {
    pub name: String,
    pub machine: Machine,
    /// The project's checkout there, `~/…` or absolute.
    pub checkout: String,
}

/// What the probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Ready,
    Offline(String),
    NotReady(String),
    Busy(String),
}

/// How a session ended, seen from here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Passed,
    Failed,
    ConnectionLost,
    /// A stop signal reached this atelier.
    Interrupted,
}

/// A finished session: the verdict, the remote runs and their results, failure lines.
#[derive(Debug, Clone)]
pub struct SessionEnd {
    pub verdict: Verdict,
    pub runs: Vec<(String, Option<RunResult>)>,
    pub failures: Vec<String>,
}

impl Remote {
    pub fn new(name: &str, machine: &Machine, checkout: &Checkout) -> Remote {
        Remote {
            name: name.to_owned(),
            machine: machine.clone(),
            checkout: format!(
                "{}/{}",
                machine.checkouts().trim_end_matches('/'),
                project_id(checkout)
            ),
        }
    }

    fn dest(&self) -> &str {
        self.machine.ssh.as_deref().expect("a remote machine")
    }

    fn ssh_program() -> String {
        std::env::var(SSH_VAR).unwrap_or_else(|_| "ssh".to_owned())
    }

    fn options(&self) -> Vec<String> {
        [
            "BatchMode=yes".to_owned(),
            format!("ConnectTimeout={}", self.machine.connect_timeout()),
            "ServerAliveInterval=5".to_owned(),
            "ServerAliveCountMax=3".to_owned(),
            "LogLevel=ERROR".to_owned(),
        ]
        .into_iter()
        .flat_map(|o| ["-o".to_owned(), o])
        .collect()
    }

    /// `ssh [-tt] <options> <destination> exec /bin/sh -c '<script>'`.
    fn ssh(&self, tty: bool, script: &str) -> Command {
        let mut cmd = Command::new(Self::ssh_program());
        if tty {
            cmd.arg("-tt");
        }
        cmd.args(self.options())
            .arg("--")
            .arg(self.dest())
            .arg(format!("exec /bin/sh -c {}", sq(script)));
        cmd
    }

    fn bin(&self) -> String {
        shell_path(&format!("~/.local/share/atelier/{VERSION}/bin/atelier"))
    }

    /// `PATH=… ` as the machine's `path` says, or nothing to keep the machine's own.
    fn path_assignment(&self) -> String {
        match &self.machine.path {
            Some(entries) => format!(
                "PATH={} ",
                entries
                    .iter()
                    .map(|e| shell_path(e))
                    .collect::<Vec<_>>()
                    .join(":")
            ),
            None => String::new(),
        }
    }

    /// A script that runs the pinned atelier's `remote-run` with `args`, or says that it is not
    /// installed.
    fn atelier_script(&self, nonce: &str, args: &[String]) -> String {
        let args: Vec<String> = args.iter().map(|a| sq(a)).collect();
        format!(
            "b={bin}; if [ -x \"$b\" ]; then exec env {path}ATELIER_DISPATCHED=1 \"$b\" remote-run {args}; else printf '%s\\n' {msg}; fi",
            bin = self.bin(),
            path = self.path_assignment(),
            args = args.join(" "),
            msg = sq(&format!(
                "{}not-ready atelier {VERSION} is not installed there: run atelier machines init {}",
                tag(nonce),
                self.name
            )),
        )
    }

    /// Connects, checks what can be checked without the commit, and the machine lock.
    pub fn probe(&self, platform: &str, settings_text: &str) -> Probe {
        let nonce = new_nonce();
        let script = self.atelier_script(
            &nonce,
            &[
                "probe".into(),
                "--name".into(),
                self.name.clone(),
                "--platform".into(),
                platform.into(),
                "--checkout".into(),
                self.checkout.clone(),
                "--nonce".into(),
                nonce.clone(),
            ],
        );
        let out = match run_with_input(self.ssh(false, &script), settings_text.as_bytes()) {
            Ok(o) => o,
            Err(e) => return Probe::Offline(format!("cannot run ssh: {e}")),
        };
        if out.status.code() == Some(255) {
            return Probe::Offline(last_line(&out.stderr));
        }
        let lines = protocol_lines(&out.stdout, &nonce);
        let mut identity = None;
        for l in &lines {
            if let Some(id) = l.strip_prefix("identity ") {
                identity = Some(id.to_owned());
            } else if let Some(r) = l.strip_prefix("not-ready ") {
                return Probe::NotReady(r.to_owned());
            } else if let Some(h) = l.strip_prefix("busy") {
                return Probe::Busy(h.trim().to_owned());
            } else if l == "ready" {
                return match identity {
                    Some(id) if id == source::identity() => Probe::Ready,
                    other => Probe::NotReady(format!(
                        "atelier there is {}, here {}: run atelier machines init {}",
                        other.as_deref().unwrap_or("unknown"),
                        source::identity(),
                        self.name
                    )),
                };
            }
        }
        Probe::NotReady(format!(
            "the machine answered without a status{}",
            match last_line(&out.stderr) {
                s if s.is_empty() => format!(" (ssh exit {})", out.status),
                s => format!(": {s}"),
            }
        ))
    }

    /// Pushes `commit` into the machine's checkout, over ssh only.
    pub fn push(&self, checkout: &Checkout, commit: &str) -> std::result::Result<(), String> {
        let url = match self.checkout.strip_prefix("~/") {
            Some(rel) => format!("{}:{rel}", self.dest()),
            None => format!("{}:{}", self.dest(), self.checkout),
        };
        let ssh_command = std::iter::once(Self::ssh_program())
            .chain(self.options())
            .map(|w| sq(&w))
            .collect::<Vec<_>>()
            .join(" ");
        let out = git::command(&checkout.root)
            .env("GIT_SSH_COMMAND", ssh_command)
            .env_remove("GIT_SSH")
            .args([
                "push",
                "--quiet",
                "--no-verify",
                "--receive-pack=git receive-pack",
                &url,
                &format!("+{commit}:{PUSH_REF}"),
            ])
            .output()
            .map_err(|e| format!("cannot run git push: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!("git push failed: {}", last_line(&out.stderr)))
        }
    }

    /// Starts `remote-run run` over `ssh -tt`; the stream goes to `log`.
    pub fn start(&self, job: &Job, commit: &str, log: &Path) -> std::io::Result<Session> {
        let nonce = new_nonce();
        let mut args: Vec<String> = vec![
            "run".into(),
            "--name".into(),
            self.name.clone(),
            "--platform".into(),
            job.platform.to_string(),
            "--checkout".into(),
            self.checkout.clone(),
            "--nonce".into(),
            nonce.clone(),
            "--commit".into(),
            commit.into(),
            "--dir".into(),
            if job.rel.as_os_str().is_empty() {
                ".".into()
            } else {
                job.rel.to_string_lossy().into_owned()
            },
        ];
        if let Some(c) = &job.command {
            args.push("--".into());
            args.extend(c.iter().cloned());
        }
        let mut cmd = self.ssh(true, &self.atelier_script(&nonce, &args));
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let log_file = File::options().create(true).append(true).open(log)?;
        let (tx, rx) = channel();
        let marker = tag(&nonce);
        let reader = std::thread::spawn(move || {
            let mut log_file = log_file;
            let mut r = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match r.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                while buf.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                    buf.pop();
                }
                let line = String::from_utf8_lossy(&buf);
                match line.strip_prefix(&marker) {
                    Some(ev) => {
                        let _ = tx.send(Event::Status(ev.to_owned()));
                    }
                    None => {
                        let _ = log_file.write_all(&buf);
                        let _ = log_file.write_all(b"\n");
                    }
                }
            }
            let _ = tx.send(Event::Eof);
        });
        let err_reader = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut BufReader::new(stderr), &mut text);
            text
        });
        Ok(Session {
            remote: self.clone(),
            child,
            _stdin: stdin,
            events: rx,
            reader: Some(reader),
            err_reader: Some(err_reader),
            runs: Vec::new(),
            done: false,
            error: None,
            eof: false,
        })
    }

    /// Reads the `result` of each run, `None` where there is none (yet).
    pub fn results(&self, ids: &[String]) -> std::result::Result<Vec<Option<RunResult>>, String> {
        let nonce = new_nonce();
        let t = tag(&nonce);
        let mut script = String::new();
        for id in ids {
            if !crate::runs::is_run_id(id) {
                return Err(format!("not a run id: {id:?}"));
            }
            script.push_str(&format!(
                "printf '%s\\n' {start}; cat {path} 2>/dev/null; printf '\\n%s\\n' {end}; ",
                start = sq(&format!("{t}result {id}")),
                path = shell_path(&format!("{}/.git/atelier/runs/{id}/result", self.checkout)),
                end = sq(&format!("{t}end")),
            ));
        }
        let out = self
            .ssh(false, &script)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run ssh: {e}"))?;
        if !out.status.success() {
            return Err(last_line(&out.stderr));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut found: BTreeMap<String, String> = BTreeMap::new();
        let mut current: Option<(String, String)> = None;
        for line in text.lines() {
            if let Some(id) = line.strip_prefix(&format!("{t}result ")) {
                current = Some((id.to_owned(), String::new()));
            } else if line == format!("{t}end") {
                if let Some((id, body)) = current.take() {
                    found.insert(id, body);
                }
            } else if let Some((_, body)) = current.as_mut() {
                body.push_str(line);
                body.push('\n');
            }
        }
        Ok(ids
            .iter()
            .map(|id| {
                found
                    .get(id)
                    .filter(|b| !b.trim().is_empty())
                    .and_then(|b| toml::from_str::<RunResult>(b).ok())
            })
            .collect())
    }

    /// `uname -s -m` there, or why not.
    pub fn uname(&self) -> std::result::Result<String, Probe> {
        let out = self
            .ssh(false, "uname -s -m")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| Probe::Offline(format!("cannot run ssh: {e}")))?;
        if out.status.code() == Some(255) {
            return Err(Probe::Offline(last_line(&out.stderr)));
        }
        if !out.status.success() {
            return Err(Probe::NotReady(format!(
                "uname failed: {}",
                last_line(&out.stderr)
            )));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut w = text.split_whitespace();
        let os = match w.next() {
            Some("Darwin") => "macos",
            Some("Linux") => "linux",
            other => return Err(Probe::NotReady(format!("uname says {other:?}"))),
        };
        let arch = match w.next() {
            Some("arm64" | "aarch64") => "aarch64",
            Some("x86_64" | "amd64") => "x86_64",
            other => return Err(Probe::NotReady(format!("uname says {other:?}"))),
        };
        Ok(format!("{os}-{arch}"))
    }

    /// Creates the project's checkout there if missing: `created` or `exists`.
    pub fn create_checkout(&self) -> std::result::Result<String, String> {
        let script = format!(
            "d={d}; if [ -d \"$d/.git\" ]; then echo exists; else mkdir -p \"$d\" && git -C \"$d\" init -q && echo created; fi",
            d = shell_path(&self.checkout)
        );
        let out = self
            .ssh(false, &script)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run ssh: {e}"))?;
        if !out.status.success() {
            return Err(last_line(&out.stderr));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// The installed pinned atelier's identity there (`<version> <hash>`), `None` if missing.
    pub fn installed_identity(&self) -> std::result::Result<Option<String>, String> {
        let script = format!(
            "b={bin}; if [ -x \"$b\" ]; then ATELIER_DISPATCHED=1 \"$b\" remote-run identity; fi",
            bin = self.bin()
        );
        let out = self
            .ssh(false, &script)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("cannot run ssh: {e}"))?;
        if !out.status.success() {
            return Err(last_line(&out.stderr));
        }
        let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        Ok((!text.is_empty()).then_some(text))
    }

    /// Builds this binary's own source there with cargo and installs it at
    /// `~/.local/share/atelier/<version>/`, replacing an install of other source. cargo's output
    /// goes to this program's stderr.
    pub fn install(&self) -> std::result::Result<(), String> {
        let script = format!(
            "set -e\n\
             root=\"$HOME/.local/share/atelier\"\n\
             v={v}\n\
             mkdir -p \"$root\"\n\
             src=$(mktemp -d \"${{TMPDIR:-/tmp}}/atelier-src.XXXXXX\")\n\
             trap 'rm -rf \"$src\"' EXIT\n\
             tar -xf - -C \"$src\"\n\
             new=\"$root/.$v.new.$$\"\n\
             rm -rf \"$new\"\n\
             cd \"$src\"\n\
             {path}export PATH\n\
             MBX_DISABLE=1 CARGO_TARGET_DIR=\"$src/target\" cargo install --locked --quiet --path . --root \"$new\"\n\
             rm -f \"$new/.crates.toml\" \"$new/.crates2.json\"\n\
             if [ -e \"$root/$v\" ]; then old=\"$root/.$v.old.$$\"; mv \"$root/$v\" \"$old\"; mv \"$new\" \"$root/$v\"; rm -rf \"$old\"; else mv \"$new\" \"$root/$v\"; fi\n",
            v = sq(VERSION),
            path = match self.path_assignment() {
                p if p.is_empty() => String::new(),
                p => format!("{}\n", p.trim_end()),
            },
        );
        let mut cmd = self.ssh(false, &script);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn().map_err(|e| format!("cannot run ssh: {e}"))?;
        let archive = source::tar();
        let mut stdin = child.stdin.take().expect("piped");
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&archive);
        });
        let status = child.wait().map_err(|e| e.to_string())?;
        let _ = writer.join();
        if status.success() {
            Ok(())
        } else {
            Err(format!("the build there failed (ssh exit {status})"))
        }
    }
}

/// A started `remote-run` session.
#[derive(Debug)]
pub struct Session {
    pub remote: Remote,
    child: Child,
    /// Held open: the remote side reads end-of-input only when the connection goes.
    _stdin: Option<ChildStdin>,
    events: Receiver<Event>,
    reader: Option<JoinHandle<()>>,
    err_reader: Option<JoinHandle<String>>,
    runs: Vec<String>,
    done: bool,
    error: Option<String>,
    eof: bool,
}

#[derive(Debug)]
enum Event {
    Status(String),
    Eof,
}

/// What the start of a session said.
#[derive(Debug)]
pub enum Start {
    Taken,
    NotReady(String),
    Busy(String),
    Offline(String),
}

impl Session {
    fn handle(&mut self, ev: Event) -> Option<Start> {
        match ev {
            Event::Eof => {
                self.eof = true;
                None
            }
            Event::Status(s) => {
                if s == "taken" {
                    Some(Start::Taken)
                } else if let Some(r) = s.strip_prefix("not-ready ") {
                    Some(Start::NotReady(r.to_owned()))
                } else if let Some(h) = s.strip_prefix("busy") {
                    Some(Start::Busy(h.trim().to_owned()))
                } else if let Some(id) = s.strip_prefix("run ") {
                    if !self.runs.iter().any(|r| r == id) {
                        self.runs.push(id.to_owned());
                    }
                    None
                } else if s == "done" {
                    self.done = true;
                    None
                } else if let Some(e) = s.strip_prefix("error ") {
                    self.error = Some(e.to_owned());
                    None
                } else {
                    None
                }
            }
        }
    }

    /// Waits for the machine to take the run or say why not.
    pub fn wait_start(&mut self) -> Start {
        loop {
            match self.events.recv_timeout(Duration::from_millis(100)) {
                Ok(ev) => {
                    if let Some(s) = self.handle(ev) {
                        return s;
                    }
                    if self.eof {
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            if signals::received().is_some() {
                self.stop();
                break;
            }
        }
        let status = self.child.wait().ok();
        let err = self.stderr_text();
        if status.and_then(|s| s.code()) == Some(255) {
            Start::Offline(last_line(err.as_bytes()))
        } else {
            Start::NotReady(format!(
                "remote-run ended without a status{}",
                match last_line(err.as_bytes()) {
                    s if s.is_empty() => String::new(),
                    s => format!(": {s}"),
                }
            ))
        }
    }

    fn stop(&mut self) {
        // SAFETY: our own unreaped child; TERM makes ssh close the connection.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
    }

    fn stderr_text(&mut self) -> String {
        self.err_reader
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }

    /// Waits for the session to end and reads the verdict. A stop signal here closes the
    /// connection, which stops the remote run.
    pub fn finish(mut self) -> SessionEnd {
        let mut stopped = false;
        let ssh_status = loop {
            while let Ok(ev) = self.events.try_recv() {
                self.handle(ev);
            }
            match self.child.try_wait() {
                Ok(Some(st)) => break Some(st),
                Ok(None) => {}
                Err(_) => break None,
            }
            if !stopped && signals::received().is_some() {
                self.stop();
                stopped = true;
            }
            sleep(Duration::from_millis(50));
        };
        // The stream ends with the connection; with a stand-in for ssh, when the remote ends.
        let until = Instant::now() + Duration::from_secs(15);
        while !self.eof && Instant::now() < until {
            match self.events.recv_timeout(Duration::from_millis(100)) {
                Ok(ev) => {
                    self.handle(ev);
                }
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        if self.eof
            && let Some(r) = self.reader.take()
        {
            let _ = r.join();
        }
        let ssh_err = if self.eof {
            self.stderr_text()
        } else {
            String::new()
        };

        let ids = self.runs.clone();
        let deadline = Instant::now() + RESULT_WAIT;
        let mut results: Vec<Option<RunResult>> = vec![None; ids.len()];
        let mut read_error: Option<String>;
        loop {
            match self.remote.results(&ids) {
                Ok(r) => {
                    results = r;
                    read_error = None;
                }
                Err(e) => read_error = Some(e),
            }
            let complete = results.iter().all(Option::is_some);
            if self.done || complete || Instant::now() >= deadline {
                break;
            }
            sleep(Duration::from_secs(1));
        }

        let mut failures = Vec::new();
        let name = &self.remote.name;
        for (id, r) in ids.iter().zip(&results) {
            match r {
                Some(r) => failures.extend(r.failures.iter().map(|f| format!("{name}: {f}"))),
                None => failures.push(format!("{name}: run {id} has no result")),
            }
        }
        if let Some(e) = &read_error {
            failures.push(format!("{name}: cannot read the results: {e}"));
        }
        let verdict = if stopped {
            Verdict::Interrupted
        } else if let Some(e) = &self.error {
            failures.push(format!("{name}: {e}"));
            Verdict::Failed
        } else if connection_lost(self.done, ssh_status, &results) {
            let why = last_line(ssh_err.as_bytes());
            if !failures
                .iter()
                .any(|f| f == &format!("{name}: connection lost"))
            {
                failures.push(if why.is_empty() {
                    format!("{name}: connection lost")
                } else {
                    format!("{name}: connection lost ({why})")
                });
            }
            Verdict::ConnectionLost
        } else if results.iter().all(|r| {
            r.as_ref()
                .is_some_and(|r| r.outcome == crate::runs::Outcome::Passed)
        }) {
            Verdict::Passed
        } else {
            Verdict::Failed
        };
        SessionEnd {
            verdict,
            runs: ids.into_iter().zip(results).collect(),
            failures,
        }
    }
}

/// The connection was lost when the session ended without `done`, when ssh itself failed (exit
/// 255) or was killed, or when a remote run says so.
fn connection_lost(
    done: bool,
    ssh: Option<std::process::ExitStatus>,
    results: &[Option<RunResult>],
) -> bool {
    use std::os::unix::process::ExitStatusExt;
    let ssh_failed = ssh.is_some_and(|s| s.code() == Some(255) || s.signal().is_some());
    let remote_says = results.iter().flatten().any(|r| {
        r.outcome == crate::runs::Outcome::Interrupted
            && r.failures.iter().any(|f| f == "connection lost")
    });
    !done || ssh_failed || remote_says
}

/// Tries one remote machine for `job` (connect, preflight, push, take): the session, or why not.
pub fn attempt(
    remote: &Remote,
    checkout: &Checkout,
    settings_text: &str,
    job: &Job,
    commit: &str,
    log: &Path,
) -> Attempt<Session> {
    match remote.probe(&job.platform.to_string(), settings_text) {
        Probe::Ready => {}
        Probe::Offline(d) => return Attempt::Offline(d),
        Probe::NotReady(l) => return Attempt::NotReady(l),
        Probe::Busy(h) => return Attempt::Busy(h),
    }
    if let Err(e) = remote.push(checkout, commit) {
        return Attempt::NotReady(format!("cannot push the commit: {e}"));
    }
    let mut session = match remote.start(job, commit, log) {
        Ok(s) => s,
        Err(e) => return Attempt::Offline(format!("cannot run ssh: {e}")),
    };
    match session.wait_start() {
        Start::Taken => Attempt::Taken(session),
        Start::NotReady(l) => Attempt::NotReady(l),
        Start::Busy(h) => Attempt::Busy(h),
        Start::Offline(d) => Attempt::Offline(d),
    }
}

fn new_nonce() -> String {
    crate::fsutil::random_hex(8).unwrap_or_else(|_| format!("{:x}", std::process::id()))
}

/// The protocol lines of a session's output, without their prefix.
fn protocol_lines(out: &[u8], nonce: &str) -> Vec<String> {
    let t = tag(nonce);
    String::from_utf8_lossy(out)
        .lines()
        .filter_map(|l| l.trim_end_matches('\r').strip_prefix(&t).map(str::to_owned))
        .collect()
}

fn last_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| l.trim())
        .rfind(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned()
}

/// Runs `cmd` with `input` on its stdin and collects its output.
fn run_with_input(mut cmd: Command, input: &[u8]) -> std::io::Result<std::process::Output> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().expect("piped");
    let data = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&data);
    });
    let out = child.wait_with_output();
    let _ = writer.join();
    out
}

/// The folder a remote run's stream is kept in, inside this computer's run folder.
pub fn log_path(run_dir: &Path, machine: &str) -> PathBuf {
    run_dir.join(format!("remote-{machine}.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_words_survive_quotes_and_home() {
        assert_eq!(sq("a'b"), "'a'\\''b'");
        assert_eq!(shell_path("~/x y/z"), "\"$HOME\"/'x y/z'");
        assert_eq!(shell_path("/abs"), "'/abs'");
        let out = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "printf '%s|' {} {}",
                sq("it's $HOME"),
                shell_path("~/a b")
            ))
            .env("HOME", "/h")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "it's $HOME|/h/a b|");
    }

    #[test]
    fn protocol_lines_need_the_session_nonce() {
        let out = b"log line\r\natelier-remote-abc: taken\r\natelier-remote-xyz: done\natelier-remote-abc: run 1\n";
        assert_eq!(protocol_lines(out, "abc"), ["taken", "run 1"]);
    }
}
