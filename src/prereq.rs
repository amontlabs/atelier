//! Tool prerequisites: what `check` verifies after the base preflight and
//! before it takes the run lock. Every problem found is reported together in one refusal (4).
//!
//! - one pinned Rust toolchain per workspace, and `rustc --version` reports it;
//! - cargo-nextest at the pinned version;
//! - each declared tool whose platforms include this one;
//! - nothing in each workspace's `.config/nextest.toml` that hides a test or lets a failing one
//!   count as passed ([`nextest::config_problems`]); the file, and nextest's user config, are read
//!   again when the run ends ([`recheck_nextest`]);
//! - with `[prerequisites.mbx]`: mbx's version and every listed setting, `<workspace root>/.mbx.toml`
//!   read from disk, nothing that tells cargo to bypass mbx, no relative `PATH` entry, cargo's
//!   target folder (`cargo metadata`) being `<workspace root>/target`, and no build folder that
//!   plain cargo made. These are checked again when the run ends ([`recheck`]).
//!
//! Every command started here carries `ATELIER_RUN=<marker>`; a process still running from one of
//! them when the checks are done is a problem, named.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::exit::{Failure, Result};
use crate::git::{self, Checkout};
use crate::nextest;
use crate::platform::Platform;
use crate::procs::{self, RUN_MARKER, RUNS_MARKER};
use crate::settings::{Mbx, Settings, Tool};

/// Environment variables that make mbx step aside (mbx §3, §4).
const MBX_BYPASS_VARS: &[&str] = &[
    "MBX_DISABLE",
    "CARGO_TARGET_DIR",
    "CARGO_BUILD_TARGET_DIR",
    "RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WRAPPER",
    "RUSTC_WORKSPACE_WRAPPER",
];

/// A command whose environment carries the run markers as a step's does: `ATELIER_RUN=<marker>`
/// and `ATELIER_RUNS` with the outer runs' ids, then the marker.
fn command(program: impl AsRef<std::ffi::OsStr>, marker: &str) -> Command {
    let mut runs = procs::inherited_runs();
    runs.retain(|id| id != marker);
    runs.push(marker.to_owned());
    let mut cmd = Command::new(program);
    cmd.env(RUN_MARKER, marker).env(RUNS_MARKER, runs.join(":"));
    cmd
}

/// One Cargo workspace named in the settings.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// The folder as written in the settings (`.` for the checkout root).
    pub rel: String,
    /// The folder, absolute.
    pub dir: PathBuf,
    /// The workspace root `cargo metadata` reports (the folder when metadata failed).
    pub root: PathBuf,
    /// The package names of the workspace members.
    pub members: Vec<String>,
    /// Why `cargo metadata` failed, if it did.
    pub metadata_error: Option<String>,
}

impl Workspace {
    /// A name usable in step and file names; empty for the checkout root.
    pub fn tag(&self) -> String {
        let t: String = self
            .rel
            .trim_matches(|c| c == '.' || c == '/')
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        t
    }
}

/// Asks `cargo metadata` for each workspace's root and members. A failure is recorded in the
/// workspace, not returned: a broken manifest is RED at the format step, named.
pub fn workspaces(checkout: &Checkout, settings: &Settings, marker: &str) -> Vec<Workspace> {
    settings
        .workspaces
        .iter()
        .map(|rel| {
            let dir = checkout.root.join(rel);
            let mut ws = Workspace {
                rel: rel.clone(),
                dir: dir.clone(),
                root: dir.clone(),
                members: Vec::new(),
                metadata_error: None,
            };
            match metadata(&dir, marker) {
                Ok(m) => {
                    ws.root = m.root;
                    ws.members = m.members;
                }
                Err(e) => ws.metadata_error = Some(e),
            }
            ws
        })
        .collect()
}

/// What `cargo metadata` says about a workspace.
struct Metadata {
    root: PathBuf,
    members: Vec<String>,
    /// Where cargo builds, after every environment variable and config file.
    target_dir: PathBuf,
}

fn metadata(dir: &Path, marker: &str) -> std::result::Result<Metadata, String> {
    let out = command("cargo", marker)
        .args(["metadata", "--no-deps", "--format-version", "1", "--locked"])
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run cargo: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err
            .lines()
            .find(|l| l.starts_with("error"))
            .or_else(|| err.lines().next())
            .unwrap_or("no message");
        return Err(format!("cargo metadata failed: {}", line.trim()));
    }
    let v: Value = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("cargo metadata printed unreadable output: {e}"))?;
    let root = v
        .get("workspace_root")
        .and_then(Value::as_str)
        .ok_or("cargo metadata has no workspace_root")?;
    let mut members: Vec<String> = v
        .get("packages")
        .and_then(Value::as_array)
        .map(|ps| {
            ps.iter()
                .filter_map(|p| p.get("name").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    members.sort();
    members.dedup();
    let target_dir = v
        .get("target_directory")
        .and_then(Value::as_str)
        .ok_or("cargo metadata has no target_directory")?;
    Ok(Metadata {
        root: PathBuf::from(root),
        members,
        target_dir: PathBuf::from(target_dir),
    })
}

/// What `check` reads again when the run ends.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Each workspace root's `.mbx.toml` as read at preflight: its bytes, `None` when absent.
    mbx_files: Vec<(PathBuf, Option<Vec<u8>>)>,
    /// Each file nextest reads configuration from, as read at preflight.
    nextest_files: Vec<(PathBuf, FileState)>,
}

/// A file's bytes (`None`: absent), or why it cannot be read.
type FileState = std::result::Result<Option<Vec<u8>>, String>;

fn file_state(path: &Path) -> FileState {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// The files nextest takes configuration from: each workspace's `.config/nextest.toml`, then
/// the user config (`NEXTEST_USER_CONFIG_FILE`, else the XDG and home locations).
fn nextest_paths(workspaces: &[Workspace]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut project: Vec<PathBuf> = Vec::new();
    for ws in workspaces {
        let file = ws.root.join(".config/nextest.toml");
        if !project.contains(&file) {
            project.push(file);
        }
    }
    let mut user = Vec::new();
    if let Some(f) = std::env::var_os("NEXTEST_USER_CONFIG_FILE") {
        user.push(PathBuf::from(f));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        user.push(PathBuf::from(xdg).join("nextest/config.toml"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        user.push(PathBuf::from(home).join(".config/nextest/config.toml"));
    }
    (project, user)
}

/// Verifies every prerequisite; one refusal (4) lists every problem.
pub fn verify(
    checkout: &Checkout,
    settings: &Settings,
    workspaces: &[Workspace],
    platform: Option<Platform>,
    marker: &str,
) -> Result<Snapshot> {
    let mut problems = Vec::new();

    if let Some(rust) = &settings.rust {
        for ws in workspaces {
            toolchain_problems(checkout, ws, marker, &mut problems);
        }
        if let Some(p) = nextest_version_problem(&checkout.root, &rust.nextest, marker) {
            problems.push(p);
        }
    }
    for tool in &settings.prerequisites.tools {
        if tool_applies(tool, platform)
            && let Some(p) = tool_problem(&checkout.root, tool, marker)
        {
            problems.push(p);
        }
    }
    let (project_files, user_files) = nextest_paths(workspaces);
    let mut nextest_files = Vec::new();
    for file in project_files.iter().chain(&user_files) {
        let state = file_state(file);
        match &state {
            Err(e) => problems.push(format!(
                "{}: cannot be read ({e}); nextest would not read it either",
                display(checkout, file)
            )),
            Ok(Some(bytes)) if project_files.contains(file) => match std::str::from_utf8(bytes) {
                Ok(text) => {
                    problems.extend(nextest::config_problems(&display(checkout, file), text))
                }
                Err(_) => problems.push(format!(
                    "{}: cannot be read (not UTF-8); nextest would not read it either",
                    display(checkout, file)
                )),
            },
            _ => {}
        }
        nextest_files.push((file.clone(), state));
    }

    let mut mbx_files = Vec::new();
    if let Some(mbx) = &settings.prerequisites.mbx {
        problems.extend(mbx_problems(checkout, mbx, workspaces, marker));
        for ws in workspaces {
            mbx_files.push((
                ws.root.join(".mbx.toml"),
                read_optional(&ws.root.join(".mbx.toml")),
            ));
        }
    }

    match procs::marker_survivors(&[marker.to_owned()]) {
        Ok(found) if !found.is_empty() => problems.push(format!(
            "a prerequisite command left processes behind: {}",
            found
                .iter()
                .map(|(p, _)| p.describe())
                .collect::<Vec<_>>()
                .join("; ")
        )),
        Ok(_) => {}
        Err(e) => problems.push(format!("cannot scan for processes left behind: {e}")),
    }

    if problems.is_empty() {
        Ok(Snapshot {
            mbx_files,
            nextest_files,
        })
    } else {
        Err(Failure::refused(format!(
            "prerequisites not met:\n  {}",
            problems.join("\n  ")
        )))
    }
}

/// What changed in the mbx checks while the run was going, one line each (RED when not empty):
/// the version, the settings, a `.mbx.toml` that appeared, changed or vanished, and a build
/// folder that is no longer absent or a link into mbx's storage.
pub fn recheck(
    checkout: &Checkout,
    settings: &Settings,
    workspaces: &[Workspace],
    snapshot: &Snapshot,
    marker: &str,
) -> Vec<String> {
    let Some(mbx) = &settings.prerequisites.mbx else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    for (path, before) in &snapshot.mbx_files {
        let now = read_optional(path);
        if &now != before {
            lines.push(format!(
                "{} {} during the run",
                display(checkout, path),
                match (before, &now) {
                    (None, _) => "appeared",
                    (_, None) => "was removed",
                    _ => "changed",
                }
            ));
        }
    }
    lines.extend(mbx_problems(checkout, mbx, workspaces, marker));
    lines
}

/// What changed in the files nextest reads configuration from while the run was going, one line
/// each (RED when not empty): a file that appeared, changed, vanished or became unreadable.
pub fn recheck_nextest(checkout: &Checkout, snapshot: &Snapshot) -> Vec<String> {
    let mut lines = Vec::new();
    for (path, before) in &snapshot.nextest_files {
        let now = file_state(path);
        if &now == before {
            continue;
        }
        let what = match (before, &now) {
            (_, Err(e)) => format!("cannot be read now ({e})"),
            (Ok(None), _) => "appeared".to_owned(),
            (_, Ok(None)) => "was removed".to_owned(),
            _ => "changed".to_owned(),
        };
        lines.push(format!(
            "{} {what} during the run; the project steps' nextest calls may not have used the file that was checked",
            display(checkout, path)
        ));
    }
    lines
}

fn tool_applies(tool: &Tool, platform: Option<Platform>) -> bool {
    match (&tool.platforms, platform) {
        (None, _) => true,
        (Some(list), Some(p)) => list.iter().any(|l| Platform::parse(l) == Some(p)),
        (Some(_), None) => false,
    }
}

/// The first output line of a command: standard output, else standard error.
fn first_line(cmd: &mut Command) -> std::result::Result<String, String> {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run: {e}"))?;
    let pick = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_owned)
    };
    if !out.status.success() {
        let why = pick(&out.stderr)
            .or_else(|| pick(&out.stdout))
            .unwrap_or_else(|| "no output".to_owned());
        return Err(format!("exited with {}: {why}", out.status));
    }
    pick(&out.stdout)
        .or_else(|| pick(&out.stderr))
        .ok_or_else(|| "printed nothing".to_owned())
}

fn tool_problem(root: &Path, tool: &Tool, marker: &str) -> Option<String> {
    let (program, args) = tool.run.split_first()?;
    let mut cmd = command(program, marker);
    cmd.args(args).current_dir(root);
    match first_line(&mut cmd) {
        Ok(line) if line.starts_with(&tool.expect) => None,
        Ok(line) => Some(format!(
            "tool {}: `{}` printed {line:?}, expected a line starting with {:?}",
            tool.name,
            tool.run.join(" "),
            tool.expect
        )),
        Err(e) => Some(format!("tool {}: `{}` {e}", tool.name, tool.run.join(" "))),
    }
}

fn nextest_version_problem(root: &Path, pinned: &str, marker: &str) -> Option<String> {
    let mut cmd = command("cargo", marker);
    cmd.args(["nextest", "--version"]).current_dir(root);
    match first_line(&mut cmd) {
        Ok(line) => {
            let version = line.split_whitespace().nth(1).unwrap_or("");
            (version != pinned).then(|| {
                format!("cargo-nextest is {version:?} ({line}); the settings pin {pinned}")
            })
        }
        Err(e) => Some(format!("cargo nextest --version {e}")),
    }
}

/// `rust-toolchain.toml` (or `rust-toolchain`) from the workspace folder up to the checkout root
/// names an exact version, and `rustc --version` in the workspace reports it.
fn toolchain_problems(
    checkout: &Checkout,
    ws: &Workspace,
    marker: &str,
    problems: &mut Vec<String>,
) {
    let at = &ws.rel;
    let mut dir = Some(ws.dir.as_path());
    let mut channel = None;
    while let Some(d) = dir {
        for name in ["rust-toolchain.toml", "rust-toolchain"] {
            if let Ok(text) = fs::read_to_string(d.join(name)) {
                channel = Some((d.join(name), toolchain_channel(&text)));
                break;
            }
        }
        if channel.is_some() || d == checkout.root {
            break;
        }
        dir = d.parent();
    }
    let Some((file, channel)) = channel else {
        problems.push(format!(
            "workspace {at}: no rust-toolchain.toml with an exact `channel`"
        ));
        return;
    };
    let file = display(checkout, &file);
    let Some(version) = channel.filter(|c| crate::settings::validate_version(c).is_ok()) else {
        problems.push(format!("{file}: `channel` is not an exact version"));
        return;
    };
    let mut cmd = command("rustc", marker);
    cmd.arg("--version").current_dir(&ws.dir);
    match first_line(&mut cmd) {
        Ok(line) if line.split_whitespace().nth(1) == Some(version.as_str()) => {}
        Ok(line) => problems.push(format!(
            "workspace {at}: rustc is {line:?}; {file} pins {version}"
        )),
        Err(e) => problems.push(format!("workspace {at}: rustc --version {e}")),
    }
}

fn toolchain_channel(text: &str) -> Option<String> {
    let trimmed = text.trim();
    // The legacy `rust-toolchain` file is just the channel name.
    if !trimmed.contains('=') && !trimmed.contains('[') {
        return Some(trimmed.to_owned());
    }
    let table: toml::Table = toml::from_str(text).ok()?;
    table
        .get("toolchain")?
        .get("channel")?
        .as_str()
        .map(str::to_owned)
}

// ----- mbx -----

fn mbx_problems(
    checkout: &Checkout,
    mbx: &Mbx,
    workspaces: &[Workspace],
    marker: &str,
) -> Vec<String> {
    let mut problems = Vec::new();

    for var in MBX_BYPASS_VARS {
        if std::env::var_os(var).is_some() {
            problems.push(format!("{var} is set: it makes cargo bypass mbx; unset it"));
        }
    }
    // A relative entry (an empty one is `.`) resolves against the folder each job runs in, not
    // against the folder atelier was started in, so which `cargo` runs cannot be told here.
    if let Some(path) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&path).filter(|e| !e.is_absolute()) {
            problems.push(format!(
                "PATH has the relative entry {:?}: it resolves differently in each folder a job runs in; use absolute entries",
                entry.display().to_string()
            ));
        }
    }
    match first_cargo() {
        None => problems.push("no `cargo` on PATH".to_owned()),
        Some(found) => {
            let listed: Vec<PathBuf> = mbx.cargo.iter().map(|c| expand_home(c)).collect();
            if !listed.iter().any(|l| normalize(l) == normalize(&found)) {
                problems.push(format!(
                    "the first `cargo` on PATH is {}; mbx needs one of: {}",
                    found.display(),
                    listed
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }

    let mut cmd = command("mbx", marker);
    cmd.arg("--version").current_dir(&checkout.root);
    match first_line(&mut cmd) {
        Ok(line) => {
            let version = line.split_whitespace().nth(1).unwrap_or("");
            if version != mbx.version {
                problems.push(format!(
                    "mbx is {version:?} ({line}); the settings pin {}",
                    mbx.version
                ));
            }
        }
        Err(e) => problems.push(format!("mbx --version {e}")),
    }
    for (key, want) in &mbx.settings {
        match mbx_setting(&checkout.root, key, marker) {
            Ok(got) if got == value_text(want) => {}
            Ok(got) => problems.push(format!(
                "mbx setting {key} is {got:?}; the settings require {}",
                value_text(want)
            )),
            Err(e) => problems.push(format!("mbx settings get {key} {e}")),
        }
    }

    let storage = mbx_target_roots(&checkout.root, marker);
    for ws in workspaces {
        let file = ws.root.join(".mbx.toml");
        match fs::read_to_string(&file) {
            Ok(text) => problems.extend(mbx_file_problems(
                &display(checkout, &file),
                &text,
                &mbx.settings,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => problems.push(format!("{}: {e}", display(checkout, &file))),
        }
        let target = ws.root.join("target");
        if ws.metadata_error.is_none() {
            match metadata(&ws.dir, marker) {
                Ok(m) if normalize(&m.target_dir) == normalize(&target) => {}
                Ok(m) => problems.push(format!(
                    "workspace {}: cargo builds in {}, not in {}; mbx manages only the workspace's `target` (a CARGO_BUILD_TARGET_DIR or `build.target-dir` setting moves it)",
                    ws.rel,
                    m.target_dir.display(),
                    display(checkout, &target)
                )),
                Err(e) => problems.push(format!(
                    "workspace {}: cannot ask cargo for its target folder: {e}",
                    ws.rel
                )),
            }
        }
        if let Some(p) = target_problem(checkout, &target, &storage) {
            problems.push(p);
        }
    }
    problems
}

/// `mbx settings get <key>`: the value as text (empty when unset).
fn mbx_setting(root: &Path, key: &str, marker: &str) -> std::result::Result<String, String> {
    let out = command("mbx", marker)
        .args(["settings", "get", key])
        .current_dir(root)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "exited with {}: {}",
            out.status,
            err.lines().next().unwrap_or("no message").trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn value_text(v: &toml::Value) -> String {
    match v {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A key of a workspace's `.mbx.toml` that is also listed in the settings must say the same.
/// Nested tables count as dotted keys (`[target] seed = true` is `target.seed`).
pub fn mbx_file_problems(
    file: &str,
    text: &str,
    listed: &BTreeMap<String, toml::Value>,
) -> Vec<String> {
    let table: toml::Table = match toml::from_str(text) {
        Ok(t) => t,
        Err(e) => {
            let msg = e.to_string();
            return vec![format!(
                "{file}: not valid TOML ({}); mbx would not read it either",
                msg.lines().next().unwrap_or("").trim()
            )];
        }
    };
    let mut flat = Vec::new();
    flatten("", &table, &mut flat);
    let mut problems = Vec::new();
    for (key, value) in flat {
        if let Some(want) = listed.get(&key)
            && &value != want
        {
            problems.push(format!(
                "{file} sets {key} = {value}; the settings require {want}"
            ));
        }
    }
    problems
}

fn flatten(prefix: &str, table: &toml::Table, out: &mut Vec<(String, toml::Value)>) {
    for (k, v) in table {
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            toml::Value::Table(t) => {
                out.push((key.clone(), v.clone()));
                flatten(&key, t, out);
            }
            _ => out.push((key, v.clone())),
        }
    }
}

/// Where mbx keeps managed build folders: `target.root`, else `<cache_dir>/targets`, else the
/// platform's default cache folder.
fn mbx_target_roots(root: &Path, marker: &str) -> Vec<PathBuf> {
    let get = |key: &str| {
        mbx_setting(root, key, marker)
            .ok()
            .filter(|v| !v.is_empty())
    };
    if let Some(r) = get("target.root") {
        return vec![PathBuf::from(r)];
    }
    if let Some(c) = get("cache_dir") {
        return vec![PathBuf::from(c).join("targets")];
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut roots = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        roots.push(PathBuf::from(xdg).join("mbx/targets"));
    }
    if let Some(home) = home {
        roots.push(home.join("Library/Caches/mbx/targets"));
        roots.push(home.join(".cache/mbx/targets"));
    }
    roots
}

/// A workspace's `target` is absent or a link into mbx's storage.
fn target_problem(checkout: &Checkout, target: &Path, storage: &[PathBuf]) -> Option<String> {
    let meta = match fs::symlink_metadata(target) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(format!("{}: {e}", display(checkout, target))),
    };
    let shown = display(checkout, target);
    if !meta.file_type().is_symlink() {
        return Some(format!(
            "{shown} is a real folder that plain cargo made; mbx needs it absent or a link into its storage"
        ));
    }
    let dest = fs::canonicalize(target).ok()?;
    let inside = storage
        .iter()
        .any(|s| dest.starts_with(git::canonical(s)) || dest.starts_with(s));
    (!inside).then(|| {
        format!(
            "{shown} links to {}, which is not in mbx's storage ({})",
            dest.display(),
            storage
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// The first executable file called `cargo` in a `PATH` folder.
fn first_cargo() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("cargo"))
        .find(|p| {
            fs::metadata(p)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(p)
}

/// A path without `.` components or trailing slashes (not resolving links).
fn normalize(p: &Path) -> PathBuf {
    p.components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect()
}

fn read_optional(path: &Path) -> Option<Vec<u8>> {
    fs::read(path).ok()
}

/// A path relative to the checkout when it is inside it.
pub fn display(checkout: &Checkout, p: &Path) -> String {
    p.strip_prefix(&checkout.root)
        .unwrap_or(p)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_carry_the_run_markers_like_steps() {
        let marker = "20261113T1012Z-4f2a09c1d3e5b7a8";
        let cmd = command("true", marker);
        let envs: BTreeMap<_, _> = cmd
            .get_envs()
            .map(|(k, v)| (k.to_owned(), v.map(|v| v.to_owned())))
            .collect();
        assert_eq!(
            envs[std::ffi::OsStr::new(RUN_MARKER)].as_deref(),
            Some(std::ffi::OsStr::new(marker))
        );
        let runs = envs[std::ffi::OsStr::new(RUNS_MARKER)].clone().unwrap();
        let runs = runs.to_str().unwrap();
        assert!(runs.split(':').next_back() == Some(marker), "{runs}");
        assert_eq!(runs.split(':').filter(|id| *id == marker).count(), 1);
    }

    fn listed() -> BTreeMap<String, toml::Value> {
        let mut m = BTreeMap::new();
        m.insert(
            "build_script_execution".to_owned(),
            toml::Value::Boolean(false),
        );
        m.insert("target.seed".to_owned(), toml::Value::Boolean(true));
        m
    }

    #[test]
    fn an_mbx_file_may_repeat_a_listed_value_but_not_change_it() {
        let f = |t: &str| mbx_file_problems(".mbx.toml", t, &listed());
        assert!(f("").is_empty());
        assert!(f("build_script_execution = false\nlinker = \"lld\"\n").is_empty());
        let got = f("build_script_execution = true\n");
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains(".mbx.toml sets build_script_execution = true"));
        assert!(!f("[target]\nseed = false\n").is_empty());
        assert!(f("[target]\nseed = true\n").is_empty());
        // A quoted key with a dot is the same listed key.
        let got = f("\"target.seed\" = false\n");
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].contains("sets target.seed = false"), "{got:?}");
        assert!(!f("= nope").is_empty());
    }

    #[test]
    fn toolchain_channels() {
        assert_eq!(
            toolchain_channel("[toolchain]\nchannel = \"1.100.0\"\ncomponents = [\"rustfmt\"]\n"),
            Some("1.100.0".to_owned())
        );
        assert_eq!(toolchain_channel("1.100.0\n"), Some("1.100.0".to_owned()));
        assert_eq!(
            toolchain_channel("[toolchain]\nprofile = \"minimal\"\n"),
            None
        );
    }

    #[test]
    fn paths_compare_without_dots_and_slashes() {
        assert_eq!(
            normalize(Path::new("/a/./b/")),
            normalize(Path::new("/a/b"))
        );
        assert_ne!(normalize(Path::new("/a/b")), normalize(Path::new("/a/c")));
    }
}
