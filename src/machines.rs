//! The owner's machines file, `~/.config/atelier/machines.toml` (DESIGN §2.5), choosing a machine
//! for a platform, and the machine-wide run lock.
//!
//! **Choosing.** The candidates are the machines of the platform (with a GUI when the platform
//! asks for one), in the project's `machines` order when it gives one, else by `priority`. Each is
//! tried in that order: connect, preflight, take its run lock. The first that takes the run runs
//! it. When none could, but at least one was ready and only busy, atelier waits for the first
//! busy one that frees up, trying each again in turn. Every machine passed over is named, with
//! its reason, in the final line.
//!
//! **The machine lock.** One run per machine at a time, whatever the project:
//! `~/.local/state/atelier/machine.lock`, an exclusive `flock` held by the atelier that runs the
//! platform's steps there (the `test` command for this computer, `remote-run` on another).
//! `machine.holder` next to it says who holds it, for messages only.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::exit::{Failure, IoContext, Result};
use crate::fsutil;
use crate::platform::{Os, Platform};
use crate::settings::PlatformSettings;

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MachinesFile {
    #[serde(default)]
    pub machines: BTreeMap<String, Machine>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Machine {
    /// This Mac itself.
    #[serde(default)]
    pub local: bool,
    /// The ssh destination of a remote machine.
    pub ssh: Option<String>,
    pub platform: String,
    #[serde(default)]
    pub gui: bool,
    /// Lower is tried first.
    pub priority: i64,
    pub connect_timeout_seconds: Option<u64>,
    /// `PATH` on a remote machine.
    pub path: Option<Vec<String>>,
    /// The folder on a remote machine that holds the projects' checkouts
    /// (default `~/.local/share/atelier/projects`).
    pub checkouts: Option<String>,
}

/// Connect timeout of a remote machine that names none.
pub const DEFAULT_CONNECT_TIMEOUT: u64 = 5;

/// Default folder of the projects' checkouts on a remote machine.
pub const DEFAULT_CHECKOUTS: &str = "~/.local/share/atelier/projects";

impl Machine {
    pub fn platform(&self) -> Platform {
        Platform::parse(&self.platform).expect("validated when parsed")
    }

    pub fn connect_timeout(&self) -> u64 {
        self.connect_timeout_seconds
            .unwrap_or(DEFAULT_CONNECT_TIMEOUT)
    }

    pub fn checkouts(&self) -> &str {
        self.checkouts.as_deref().unwrap_or(DEFAULT_CHECKOUTS)
    }

    /// Where it is, for listings: `local` or `ssh <destination>`.
    pub fn place(&self) -> String {
        match &self.ssh {
            Some(h) => format!("ssh {h}"),
            None => "local".to_owned(),
        }
    }
}

/// The file's place for a given home folder.
pub fn path_in(home: &Path) -> PathBuf {
    home.join(".config/atelier/machines.toml")
}

impl MachinesFile {
    pub fn parse(text: &str) -> Result<MachinesFile> {
        let file: MachinesFile = toml::from_str(text)
            .map_err(|e| Failure::refused(format!("machines.toml: {}", e.to_string().trim())))?;
        file.validate()?;
        Ok(file)
    }

    pub fn load(path: &Path) -> Result<MachinesFile> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Failure::refused(format!("{}: {e}", path.display())))?;
        Self::parse(&text).map_err(|f| {
            Failure::refused(
                f.message
                    .replacen("machines.toml", &path.display().to_string(), 1),
            )
        })
    }

    fn validate(&self) -> Result<()> {
        let mut problems = Vec::new();
        let mut locals = 0;
        for (name, m) in &self.machines {
            let at = format!("machines.{name}");
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
            {
                problems.push(format!("{at}: names use letters, digits, '-', '_', '.'"));
            }
            if Platform::parse(&m.platform).is_none() {
                problems.push(format!("{at}.platform: {:?} is not a platform", m.platform));
            }
            match (m.local, &m.ssh) {
                (true, Some(_)) => problems.push(format!("{at}: local and ssh are exclusive")),
                (false, None) => {
                    problems.push(format!("{at}: needs local = true or ssh = \"<host>\""))
                }
                (false, Some(host)) if host.trim().is_empty() || host.starts_with('-') => {
                    problems.push(format!("{at}.ssh: {host:?} is not a host"))
                }
                _ => {}
            }
            if m.local {
                locals += 1;
                if m.connect_timeout_seconds.is_some() || m.path.is_some() || m.checkouts.is_some()
                {
                    problems.push(format!(
                        "{at}: connect_timeout_seconds, path and checkouts are for remote machines only"
                    ));
                }
            }
            if let Some(path) = &m.path
                && path.iter().any(|p| {
                    p.is_empty() || p.contains(':') || !(p.starts_with('/') || p.starts_with("~/"))
                })
            {
                problems.push(format!(
                    "{at}.path: each entry is an absolute folder or starts with ~/, without ':'"
                ));
            }
            if let Some(c) = &m.checkouts
                && (!(c.starts_with('/') || c.starts_with("~/")) || c.contains('\n'))
            {
                problems.push(format!(
                    "{at}.checkouts: an absolute folder or one that starts with ~/"
                ));
            }
            if m.connect_timeout_seconds == Some(0) {
                problems.push(format!("{at}.connect_timeout_seconds: must be at least 1"));
            }
        }
        if locals > 1 {
            problems.push("more than one machine says local = true".to_owned());
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(Failure::refused(format!(
                "machines.toml is not valid:\n  {}",
                problems.join("\n  ")
            )))
        }
    }
}

impl MachinesFile {
    /// The owner's file in `home`. A missing file is refused (4), naming it.
    pub fn load_owner(home: &Path) -> Result<MachinesFile> {
        let path = path_in(home);
        if !path.is_file() {
            return Err(Failure::refused(format!(
                "no machines file: {} names the machines that run each platform (DESIGN §2.5)",
                path.display()
            )));
        }
        Self::load(&path)
    }

    /// One machine by name, or a refusal naming the file's machines.
    pub fn get(&self, name: &str) -> Result<&Machine> {
        self.machines.get(name).ok_or_else(|| {
            Failure::refused(format!(
                "no machine {name:?} in machines.toml (it has: {})",
                self.machines.keys().cloned().collect::<Vec<_>>().join(", ")
            ))
        })
    }
}

/// Refuses (4) a Windows platform: atelier's machine side (process groups, the survivor scan,
/// the `ssh -tt` hang-up) is built for macOS and Linux only.
pub fn refuse_windows(platform: Platform) -> Result<()> {
    if platform.os == Os::Windows {
        return Err(Failure::refused(format!(
            "{platform}: Windows machines are not supported (process groups, the survivor scan and the ssh hang-up are built for macOS and Linux)"
        )));
    }
    Ok(())
}

/// A machine that may run a platform.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub name: String,
    pub machine: Machine,
}

/// Why a machine did not run the platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// No answer within the connect timeout (the detail is ssh's).
    Offline(String),
    /// Its preflight failed: the first failing line.
    NotReady(String),
    /// Its run lock is held (the detail says by what, when known).
    Busy(String),
    /// Named by the project but not a candidate: not in the file, another platform, no GUI.
    Excluded(String),
}

impl Skip {
    /// `offline`, `not ready: <line>`, `busy`, or the exclusion: the words after the machine's
    /// name in the final line.
    pub fn reason(&self) -> String {
        match self {
            Skip::Offline(_) => "offline".to_owned(),
            Skip::NotReady(l) => format!("not ready: {l}"),
            Skip::Busy(_) => "busy".to_owned(),
            Skip::Excluded(why) => why.clone(),
        }
    }

    /// The reason with its detail, for the listing of exit 3.
    pub fn detail(&self) -> String {
        match self {
            Skip::Offline(d) | Skip::Busy(d) if !d.is_empty() => format!("{} ({d})", self.reason()),
            _ => self.reason(),
        }
    }
}

/// The machines for `platform` in the order they are tried: each a candidate, or a machine that
/// is named but cannot run it (the project names it, or it lacks the GUI the platform asks for).
pub fn candidates(
    file: &MachinesFile,
    platform: Platform,
    settings: &PlatformSettings,
) -> Vec<std::result::Result<Candidate, (String, Skip)>> {
    // A machine of another platform is named only when the project named it.
    let consider =
        |name: &str, m: &Machine, named: bool| -> Option<std::result::Result<Candidate, Skip>> {
            if m.platform() != platform {
                named.then(|| Err(Skip::Excluded(format!("is {}", m.platform))))
            } else if settings.gui && !m.gui {
                Some(Err(Skip::Excluded("has no GUI".to_owned())))
            } else {
                Some(Ok(Candidate {
                    name: name.to_owned(),
                    machine: m.clone(),
                }))
            }
        };
    let mut verdicts: Vec<(String, Option<std::result::Result<Candidate, Skip>>)> = Vec::new();
    match &settings.machines {
        Some(order) => {
            for name in order {
                let v = match file.machines.get(name) {
                    Some(m) => consider(name, m, true),
                    None => Some(Err(Skip::Excluded("is not in machines.toml".to_owned()))),
                };
                verdicts.push((name.clone(), v));
            }
        }
        None => {
            let mut all: Vec<(&String, &Machine)> = file.machines.iter().collect();
            all.sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then(a.0.cmp(b.0)));
            for (name, m) in all {
                verdicts.push((name.clone(), consider(name, m, false)));
            }
        }
    }
    verdicts
        .into_iter()
        .filter_map(|(name, v)| v.map(|r| r.map_err(|skip| (name, skip))))
        .collect()
}

/// What trying one machine gave.
#[derive(Debug)]
pub enum Attempt<T> {
    /// It took the run: the handle to run it with (a held lock, a started session).
    Taken(T),
    Offline(String),
    NotReady(String),
    Busy(String),
}

/// How machines are tried; the real one connects, the tests' ones are scripted.
pub trait Prober {
    type Taken;
    /// Connect, preflight, take the machine's run lock, in that order.
    fn attempt(&mut self, c: &Candidate) -> Attempt<Self::Taken>;
    /// Pauses before the busy machines are tried again. False stops waiting (a stop signal).
    fn pause(&mut self, busy: &[&str]) -> bool;
}

/// The outcome of choosing: the machine that took the run, if any, and every machine passed
/// over, in the order tried.
#[derive(Debug)]
pub struct Selection<T> {
    pub taken: Option<(Candidate, T)>,
    pub skipped: Vec<(String, Skip)>,
}

/// Tries the candidates in order (DESIGN §2.5). When none takes the run and at least one was
/// only busy, waits for the first busy one that frees up, trying each again in turn; a busy
/// machine that is then offline or not ready is dropped. Machines that cannot run the platform
/// are named in their place.
pub fn select<P: Prober>(
    candidates: &[std::result::Result<Candidate, (String, Skip)>],
    prober: &mut P,
) -> Selection<P::Taken> {
    let mut skipped = Vec::new();
    let mut busy: Vec<&Candidate> = Vec::new();
    for c in candidates {
        let c = match c {
            Ok(c) => c,
            Err(skip) => {
                skipped.push(skip.clone());
                continue;
            }
        };
        match prober.attempt(c) {
            Attempt::Taken(t) => {
                return Selection {
                    taken: Some((c.clone(), t)),
                    skipped,
                };
            }
            Attempt::Offline(d) => skipped.push((c.name.clone(), Skip::Offline(d))),
            Attempt::NotReady(l) => skipped.push((c.name.clone(), Skip::NotReady(l))),
            Attempt::Busy(d) => {
                skipped.push((c.name.clone(), Skip::Busy(d)));
                busy.push(c);
            }
        }
    }
    while !busy.is_empty() {
        let names: Vec<&str> = busy.iter().map(|c| c.name.as_str()).collect();
        if !prober.pause(&names) {
            break;
        }
        let mut still = Vec::new();
        for c in busy {
            let skip = match prober.attempt(c) {
                Attempt::Taken(t) => {
                    skipped.retain(|(n, _)| n != &c.name);
                    return Selection {
                        taken: Some((c.clone(), t)),
                        skipped,
                    };
                }
                Attempt::Busy(d) => {
                    still.push(c);
                    Skip::Busy(d)
                }
                Attempt::Offline(d) => Skip::Offline(d),
                Attempt::NotReady(l) => Skip::NotReady(l),
            };
            if let Some(entry) = skipped.iter_mut().find(|(n, _)| n == &c.name) {
                entry.1 = skip;
            }
        }
        busy = still;
    }
    Selection {
        taken: None,
        skipped,
    }
}

/// How often busy machines are tried again while waiting.
pub const BUSY_POLL: Duration = Duration::from_secs(5);

/// `<name> <reason>, …` for the final line.
pub fn skips_text(skipped: &[(String, Skip)]) -> String {
    skipped
        .iter()
        .map(|(n, s)| format!("{n} {}", s.reason()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The machine-wide run lock, held until dropped.
#[derive(Debug)]
pub struct MachineLock {
    _file: File,
    holder: PathBuf,
}

impl Drop for MachineLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.holder);
    }
}

/// `~/.local/state/atelier`.
pub fn state_dir(home: &Path) -> PathBuf {
    home.join(".local/state/atelier")
}

impl MachineLock {
    /// Takes the lock without waiting: `Ok(None)` when another atelier holds it. `holder` is
    /// written next to it for messages.
    pub fn try_take(home: &Path, holder: &str) -> Result<Option<MachineLock>> {
        let dir = state_dir(home);
        fs::create_dir_all(&dir).ctx(dir.display())?;
        let path = dir.join("machine.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .ctx(path.display())?;
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => return Ok(None),
            Err(fs::TryLockError::Error(e)) => {
                return Err(Failure::refused(format!("{}: {e}", path.display())));
            }
        }
        let holder_path = dir.join("machine.holder");
        let line = format!(
            "pid {} since {}: {holder}\n",
            std::process::id(),
            fsutil::rfc3339(fsutil::now_secs())
        );
        fsutil::write_atomic(&holder_path, line.as_bytes()).ctx(holder_path.display())?;
        Ok(Some(MachineLock {
            _file: file,
            holder: holder_path,
        }))
    }

    /// Who holds the lock, as written by the holder; empty when unknown.
    pub fn holder(home: &Path) -> String {
        fs::read_to_string(state_dir(home).join("machine.holder"))
            .map(|s| s.trim().to_owned())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
[machines.mac]
local = true                 # this Mac itself
platform = "macos-aarch64"
gui = true
priority = 100               # lower is tried first

[machines.linux-box]
ssh = "10.77.77.2"
platform = "linux-x86_64"
gui = false
priority = 10
connect_timeout_seconds = 3
path = ["~/.local/bin", "~/.cargo/bin", "/usr/local/bin", "/usr/bin", "/bin"]
"#;

    #[test]
    fn parses_the_design_example() {
        let f = MachinesFile::parse(EXAMPLE).unwrap();
        assert_eq!(f.machines.len(), 2);
        let linux = &f.machines["linux-box"];
        assert_eq!(linux.ssh.as_deref(), Some("10.77.77.2"));
        assert_eq!(linux.priority, 10);
        assert!(f.machines["mac"].local);
    }

    #[test]
    fn windows_is_accepted_in_the_file() {
        let f = MachinesFile::parse(
            "[machines.w]\nssh = \"w\"\nplatform = \"windows-x86_64\"\npriority = 1\n",
        );
        assert!(f.is_ok());
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        for bad in [
            "[machines.a]\nlocal = true\nplatform = \"macos-aarch64\"\npriority = 1\ncolour = 1\n",
            "[machines.a]\nplatform = \"macos-aarch64\"\npriority = 1\n",
            "[machines.a]\nlocal = true\nssh = \"x\"\nplatform = \"macos-aarch64\"\npriority = 1\n",
            "[machines.a]\nssh = \"x\"\nplatform = \"darwin\"\npriority = 1\n",
            "[machines.a]\nssh = \"-oProxyCommand=x\"\nplatform = \"linux-x86_64\"\npriority = 1\n",
            "[machines.a]\nlocal = true\nplatform = \"macos-aarch64\"\n",
            "[machines.a]\nlocal = true\nplatform = \"macos-aarch64\"\npriority = 1\n[machines.b]\nlocal = true\nplatform = \"macos-aarch64\"\npriority = 2\n",
            "[other]\n",
        ] {
            let err = MachinesFile::parse(bad).unwrap_err();
            assert_eq!(err.exit, crate::exit::Exit::Refused, "{bad}");
        }
    }

    // ----- choosing a machine -----

    use std::collections::{HashMap, VecDeque};

    fn file(text: &str) -> MachinesFile {
        MachinesFile::parse(text).unwrap()
    }

    fn platform(machines: Option<&[&str]>, gui: bool) -> PlatformSettings {
        PlatformSettings {
            run: crate::settings::PlatformRun::Named("check".into()),
            prepare: None,
            machines: machines.map(|m| m.iter().map(|s| s.to_string()).collect()),
            gui,
        }
    }

    const LINUX: &str = r#"
[machines.a]
ssh = "a"
platform = "linux-x86_64"
priority = 30
[machines.b]
ssh = "b"
platform = "linux-x86_64"
priority = 10
[machines.c]
ssh = "c"
platform = "linux-x86_64"
gui = true
priority = 20
[machines.mac]
local = true
platform = "macos-aarch64"
gui = true
priority = 1
"#;

    fn linux() -> Platform {
        Platform::parse("linux-x86_64").unwrap()
    }

    type Entries = Vec<std::result::Result<Candidate, (String, Skip)>>;

    fn names(c: &Entries) -> Vec<String> {
        c.iter()
            .map(|e| match e {
                Ok(c) => c.name.clone(),
                Err((n, s)) => format!("{n} {}", s.reason()),
            })
            .collect()
    }

    /// Answers each machine from a script, one answer per attempt (the last one repeats), and
    /// records the attempts and pauses.
    #[derive(Default)]
    struct Fake {
        script: HashMap<String, VecDeque<&'static str>>,
        tried: Vec<String>,
        pauses: usize,
        max_pauses: usize,
    }

    impl Fake {
        fn new(script: &[(&str, &[&'static str])]) -> Fake {
            Fake {
                script: script
                    .iter()
                    .map(|(n, a)| (n.to_string(), a.iter().copied().collect()))
                    .collect(),
                max_pauses: 100,
                ..Fake::default()
            }
        }
    }

    impl Prober for Fake {
        type Taken = String;
        fn attempt(&mut self, c: &Candidate) -> Attempt<String> {
            self.tried.push(c.name.clone());
            let q = self.script.get_mut(&c.name).expect("scripted");
            let a = if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                q[0]
            };
            match a {
                "take" => Attempt::Taken(c.name.clone()),
                "offline" => Attempt::Offline("timed out".into()),
                "busy" => Attempt::Busy(String::new()),
                other => Attempt::NotReady(other.into()),
            }
        }
        fn pause(&mut self, _busy: &[&str]) -> bool {
            self.pauses += 1;
            self.pauses <= self.max_pauses
        }
    }

    #[test]
    fn candidates_follow_priority_and_leave_out_other_platforms() {
        let c = candidates(&file(LINUX), linux(), &platform(None, false));
        assert_eq!(names(&c), ["b", "c", "a"]);
    }

    #[test]
    fn the_projects_order_overrides_priority_and_names_what_it_cannot_use() {
        let c = candidates(
            &file(LINUX),
            linux(),
            &platform(Some(&["a", "mac", "nope", "b"]), false),
        );
        assert_eq!(
            names(&c),
            [
                "a",
                "mac is macos-aarch64",
                "nope is not in machines.toml",
                "b"
            ]
        );
    }

    #[test]
    fn a_gui_platform_takes_only_machines_with_a_gui() {
        let c = candidates(&file(LINUX), linux(), &platform(None, true));
        assert_eq!(names(&c), ["b has no GUI", "c", "a has no GUI"]);
    }

    #[test]
    fn the_first_machine_that_takes_the_run_wins_and_earlier_ones_are_named() {
        let c = candidates(&file(LINUX), linux(), &platform(None, false));
        let mut f = Fake::new(&[
            ("b", &["offline"]),
            ("c", &["node is v22, need v24."]),
            ("a", &["take"]),
        ]);
        let s = select(&c, &mut f);
        assert_eq!(s.taken.as_ref().unwrap().1, "a");
        assert_eq!(
            skips_text(&s.skipped),
            "b offline, c not ready: node is v22, need v24."
        );
        assert_eq!(f.pauses, 0);
    }

    #[test]
    fn no_wait_when_nothing_was_only_busy() {
        let c = candidates(&file(LINUX), linux(), &platform(None, false));
        let mut f = Fake::new(&[
            ("b", &["offline"]),
            ("c", &["no checkout"]),
            ("a", &["offline"]),
        ]);
        let s = select(&c, &mut f);
        assert!(s.taken.is_none());
        assert_eq!(
            f.pauses, 0,
            "offline and not-ready machines are never waited for"
        );
        assert_eq!(
            s.skipped
                .iter()
                .map(|(_, s)| s.detail())
                .collect::<Vec<_>>(),
            [
                "offline (timed out)",
                "not ready: no checkout",
                "offline (timed out)"
            ]
        );
    }

    #[test]
    fn waits_only_when_a_ready_machine_is_busy_and_takes_the_first_that_frees() {
        let c = candidates(&file(LINUX), linux(), &platform(None, false));
        // b busy twice then free; a busy throughout; c offline. b is retried first each round.
        let mut f = Fake::new(&[
            ("b", &["busy", "busy", "busy", "take"]),
            ("c", &["offline"]),
            ("a", &["busy"]),
        ]);
        let s = select(&c, &mut f);
        assert_eq!(s.taken.as_ref().unwrap().0.name, "b");
        assert_eq!(f.pauses, 3);
        assert_eq!(f.tried, ["b", "c", "a", "b", "a", "b", "a", "b"]);
        // b ran, so it is not named; the others are, with their last reason.
        assert_eq!(skips_text(&s.skipped), "c offline, a busy");
    }

    #[test]
    fn a_later_busy_machine_that_frees_first_is_taken() {
        let c = candidates(&file(LINUX), linux(), &platform(None, false));
        let mut f = Fake::new(&[
            ("b", &["busy"]),
            ("c", &["busy", "take"]),
            ("a", &["offline"]),
        ]);
        let s = select(&c, &mut f);
        assert_eq!(s.taken.as_ref().unwrap().0.name, "c");
        assert_eq!(skips_text(&s.skipped), "b busy, a offline");
    }

    #[test]
    fn a_busy_machine_that_goes_offline_while_waited_for_ends_the_wait() {
        let c = candidates(&file(LINUX), linux(), &platform(Some(&["b", "a"]), false));
        let mut f = Fake::new(&[("b", &["busy", "offline"]), ("a", &["busy", "x"])]);
        let s = select(&c, &mut f);
        assert!(s.taken.is_none());
        assert_eq!(f.pauses, 1);
        assert_eq!(skips_text(&s.skipped), "b offline, a not ready: x");
    }

    #[test]
    fn a_stop_while_waiting_ends_without_a_machine() {
        let c = candidates(&file(LINUX), linux(), &platform(None, false));
        let mut f = Fake::new(&[("b", &["busy"]), ("c", &["busy"]), ("a", &["busy"])]);
        f.max_pauses = 2;
        let s = select(&c, &mut f);
        assert!(s.taken.is_none());
        assert_eq!(f.pauses, 3);
        assert_eq!(skips_text(&s.skipped), "b busy, c busy, a busy");
    }

    #[test]
    fn windows_is_refused_when_chosen() {
        assert!(refuse_windows(linux()).is_ok());
        let e = refuse_windows(Platform::parse("windows-x86_64").unwrap()).unwrap_err();
        assert_eq!(e.exit, crate::exit::Exit::Refused);
    }

    #[test]
    fn remote_only_keys_and_paths_are_checked() {
        for bad in [
            "[machines.a]\nlocal = true\nplatform = \"macos-aarch64\"\npriority = 1\ncheckouts = \"~/x\"\n",
            "[machines.a]\nssh = \"h\"\nplatform = \"linux-x86_64\"\npriority = 1\npath = [\"bin\"]\n",
            "[machines.a]\nssh = \"h\"\nplatform = \"linux-x86_64\"\npriority = 1\npath = [\"/a:/b\"]\n",
            "[machines.a]\nssh = \"h\"\nplatform = \"linux-x86_64\"\npriority = 1\ncheckouts = \"rel\"\n",
        ] {
            assert!(MachinesFile::parse(bad).is_err(), "{bad}");
        }
        let ok = file(
            "[machines.a]\nssh = \"h\"\nplatform = \"linux-x86_64\"\npriority = 1\ncheckouts = \"~/atelier-exp/c\"\n",
        );
        assert_eq!(ok.machines["a"].checkouts(), "~/atelier-exp/c");
        assert_eq!(ok.machines["a"].connect_timeout(), DEFAULT_CONNECT_TIMEOUT);
    }

    #[test]
    fn the_machine_lock_is_one_holder_at_a_time() {
        let home = tempfile::tempdir().unwrap();
        let first = MachineLock::try_take(home.path(), "project p")
            .unwrap()
            .unwrap();
        assert!(MachineLock::holder(home.path()).ends_with("project p"));
        assert!(
            MachineLock::try_take(home.path(), "other")
                .unwrap()
                .is_none()
        );
        drop(first);
        assert_eq!(MachineLock::holder(home.path()), "");
        assert!(
            MachineLock::try_take(home.path(), "other")
                .unwrap()
                .is_some()
        );
    }
}
