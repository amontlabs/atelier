//! `.atelier/atelier.toml`. Unknown keys are an error; every named path must exist.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::exit::{Failure, Result};
use crate::names;
use crate::platform::Platform;

/// Where the settings live, relative to the checkout root.
pub const SETTINGS_PATH: &str = ".atelier/atelier.toml";

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// The pinned atelier version.
    pub atelier: String,
    #[serde(default = "default_integration_branch")]
    pub integration_branch: String,
    #[serde(default)]
    pub workspaces: Vec<String>,
    #[serde(default = "default_promotion_wait")]
    pub promotion_wait_seconds: u64,
    pub rust: Option<Rust>,
    #[serde(default)]
    pub prerequisites: Prerequisites,
    #[serde(default)]
    pub check: Check,
    #[serde(default)]
    pub merge: Merge,
    #[serde(default)]
    pub platforms: BTreeMap<String, PlatformSettings>,
    #[serde(default)]
    pub rules: RulesSettings,
}

/// `[rules]`: where the generated `@AGENTS.md` import line goes.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RulesSettings {
    /// Files in the checkout root that must contain the line `@AGENTS.md`.
    #[serde(default = "default_import_into")]
    pub import_into: Vec<String>,
}

impl Default for RulesSettings {
    fn default() -> Self {
        RulesSettings {
            import_into: default_import_into(),
        }
    }
}

fn default_import_into() -> Vec<String> {
    vec!["CLAUDE.md".to_owned()]
}

fn default_integration_branch() -> String {
    "main".to_owned()
}

fn default_promotion_wait() -> u64 {
    300
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Rust {
    pub nextest: String,
    #[serde(default)]
    pub exclude: Vec<Exclude>,
}

/// A test deliberately not run: a nextest filterset and the required reason.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Exclude {
    pub filter: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Prerequisites {
    pub mbx: Option<Mbx>,
    #[serde(default)]
    pub tools: Vec<Tool>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Mbx {
    pub version: String,
    /// The files `cargo` on `PATH` may be (machine paths, checked as prerequisites, not here).
    pub cargo: Vec<String>,
    #[serde(default)]
    pub settings: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Tool {
    pub name: String,
    pub run: Vec<String>,
    pub expect: String,
    pub platforms: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Check {
    #[serde(default)]
    pub prepare: Vec<Hook>,
    #[serde(default)]
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub name: String,
    pub run: Vec<String>,
    pub platforms: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub name: String,
    pub run: Vec<String>,
    #[serde(default)]
    pub compile: bool,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Merge {
    #[serde(default)]
    pub after: Vec<AfterHook>,
    /// `"mergiraf"`, off when absent.
    pub resolver: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AfterHook {
    pub name: String,
    pub run: Vec<String>,
    pub commit: Option<String>,
    /// Paths this hook rewrites: a gate merge whose conflicts are only in these is settled by
    /// taking the incoming side, and the hook regenerates them.
    #[serde(default)]
    pub regenerates: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlatformSettings {
    pub run: PlatformRun,
    pub prepare: Option<Vec<String>>,
    pub machines: Option<Vec<String>>,
    #[serde(default)]
    pub gui: bool,
}

/// A platform's run: the word `"check"` (the full `atelier check`) or a command.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum PlatformRun {
    Named(String),
    Command(Vec<String>),
}

/// Reads only the pin from settings text, ignoring every other key, so that a dispatcher or an
/// older version can tell which version a newer settings file wants.
pub fn read_pin(text: &str) -> Result<String> {
    let table: toml::Table = toml::from_str(text)
        .map_err(|e| Failure::refused(format!("{SETTINGS_PATH}: {}", one_line(&e.to_string()))))?;
    let pin = table
        .get("atelier")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            Failure::refused(format!("{SETTINGS_PATH}: no `atelier = \"<version>\"` pin"))
        })?;
    validate_version(pin).map_err(|m| Failure::refused(format!("{SETTINGS_PATH}: {m}")))?;
    Ok(pin.to_owned())
}

/// Reads only `integration_branch` (default `main`) from settings text, ignoring every other key,
/// so that the branch can be read from any version's committed settings.
pub fn read_integration_branch(text: &str) -> Result<String> {
    let table: toml::Table = toml::from_str(text)
        .map_err(|e| Failure::refused(format!("{SETTINGS_PATH}: {}", one_line(&e.to_string()))))?;
    match table.get("integration_branch") {
        None => Ok(default_integration_branch()),
        Some(v) => v.as_str().map(str::to_owned).ok_or_else(|| {
            Failure::refused(format!(
                "{SETTINGS_PATH}: integration_branch is not a string"
            ))
        }),
    }
}

/// A version string usable as one path segment: `MAJOR.MINOR.PATCH`, optionally `-<pre-release>`.
pub fn validate_version(v: &str) -> std::result::Result<(), String> {
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) => (c, Some(p)),
        None => (v, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    let core_ok = parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    let pre_ok = pre.is_none_or(|p| {
        !p.is_empty()
            && p.split('.').all(|id| {
                !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
    });
    if core_ok && pre_ok {
        Ok(())
    } else {
        Err(format!(
            "invalid atelier version {v:?}: expected MAJOR.MINOR.PATCH"
        ))
    }
}

/// Why `name` cannot be a `rules.import_into` file in `root`, if it cannot: it must be one plain
/// file name that is neither under `.git` nor `AGENTS.md` itself (the same name in other letters
/// on a volume that ignores case, or another name for the same file; a symbolic link to
/// `AGENTS.md` is fine).
fn import_into_problem(root: &Path, name: &str) -> Option<&'static str> {
    let mut parts = Path::new(name).components();
    let single = matches!(
        (parts.next(), parts.next()),
        (Some(Component::Normal(_)), None)
    );
    if !single || name.contains(['/', '\\']) {
        return Some(
            "must be one file name in the checkout root (no folders, absolute paths, `.` or `..`)",
        );
    }
    if name.eq_ignore_ascii_case(".git") {
        return Some("must not be .git: it belongs to git");
    }
    if name == "AGENTS.md" {
        return Some("must be a file other than AGENTS.md");
    }
    let alias = name.eq_ignore_ascii_case("AGENTS.md")
        && root.join(".git").exists()
        && root.join(".GIT").exists();
    let path = root.join(name);
    let is_link = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
    let agents = root.join("AGENTS.md");
    let hard_alias =
        !is_link(&path) && !is_link(&agents) && crate::fsutil::same_file(&path, &agents);
    if alias || hard_alias {
        return Some("must be a file other than AGENTS.md (it is the same file on this volume)");
    }
    None
}

impl Settings {
    /// Parses settings text: syntax and unknown keys only.
    pub fn parse(text: &str) -> Result<Settings> {
        toml::from_str(text)
            .map_err(|e| Failure::refused(format!("{SETTINGS_PATH}: {}", one_line(&e.to_string()))))
    }

    /// Reads, parses and validates the settings of the checkout at `root`.
    pub fn load(root: &Path) -> Result<Settings> {
        let path = root.join(SETTINGS_PATH);
        let text = std::fs::read_to_string(&path)
            .map_err(|e| Failure::refused(format!("{SETTINGS_PATH}: {e}")))?;
        let settings = Settings::parse(&text)?;
        settings.validate(root)?;
        Ok(settings)
    }

    /// Checks everything the types cannot: names, platforms, and that every named path exists
    /// under `root`. All problems are reported together.
    pub fn validate(&self, root: &Path) -> Result<()> {
        let mut problems = Vec::new();
        if let Err(m) = validate_version(&self.atelier) {
            problems.push(m);
        }
        if names::validate_branch(root, &self.integration_branch).is_err() {
            problems.push(format!(
                "integration_branch {:?} is not a valid branch name",
                self.integration_branch
            ));
        }

        for f in &self.rules.import_into {
            if let Some(why) = import_into_problem(root, f) {
                problems.push(format!("rules.import_into: {f:?} {why}"));
            }
        }

        if self.platforms.is_empty() {
            problems
                .push("no [platforms.<os>-<arch>] table: name at least one platform".to_owned());
        }
        let declared: BTreeSet<&str> = self.platforms.keys().map(String::as_str).collect();
        for (name, p) in &self.platforms {
            if Platform::parse(name).is_none() {
                problems.push(format!(
                    "platforms.{name}: not a platform (macos|linux|windows)-(aarch64|x86_64)"
                ));
            }
            match &p.run {
                PlatformRun::Named(word) if word != "check" => problems.push(format!(
                    "platforms.{name}.run: {word:?} is neither \"check\" nor a command list"
                )),
                PlatformRun::Named(_) => {}
                PlatformRun::Command(argv) => {
                    command_problems(root, &format!("platforms.{name}.run"), argv, &mut problems)
                }
            }
            if let Some(argv) = &p.prepare {
                command_problems(
                    root,
                    &format!("platforms.{name}.prepare"),
                    argv,
                    &mut problems,
                );
            }
            if let Some(machines) = &p.machines
                && (machines.is_empty() || machines.iter().any(|m| m.trim().is_empty()))
            {
                problems.push(format!(
                    "platforms.{name}.machines: empty name or empty list"
                ));
            }
        }

        if !self.workspaces.is_empty() && self.rust.is_none() {
            problems
                .push("workspaces are named but [rust] (nextest version) is missing".to_owned());
        }
        for ws in &self.workspaces {
            match project_path(root, ws) {
                Err(m) => problems.push(format!("workspaces: {m}")),
                Ok(p) if !p.join("Cargo.toml").is_file() => {
                    problems.push(format!("workspaces: {ws}/Cargo.toml does not exist"))
                }
                Ok(_) => {}
            }
        }
        if let Some(rust) = &self.rust {
            if validate_version(&rust.nextest).is_err() {
                problems.push(format!(
                    "rust.nextest: {:?} is not an exact version",
                    rust.nextest
                ));
            }
            for (i, ex) in rust.exclude.iter().enumerate() {
                if ex.filter.trim().is_empty() || ex.reason.trim().is_empty() {
                    problems.push(format!(
                        "rust.exclude[{i}]: filter and reason are both required"
                    ));
                }
            }
        }

        if let Some(mbx) = &self.prerequisites.mbx {
            if validate_version(&mbx.version).is_err() {
                problems.push(format!(
                    "prerequisites.mbx.version: {:?} is not an exact version",
                    mbx.version
                ));
            }
            if mbx.cargo.is_empty() {
                problems.push("prerequisites.mbx.cargo: name at least one file".to_owned());
            }
        }
        let mut seen = BTreeSet::new();
        for t in &self.prerequisites.tools {
            let at = format!("prerequisites.tools {:?}", t.name);
            name_problems(&at, &t.name, &mut seen, &mut problems);
            command_problems(root, &at, &t.run, &mut problems);
            platform_list_problems(&at, t.platforms.as_deref(), &declared, &mut problems);
        }

        let mut seen = BTreeSet::new();
        for h in &self.check.prepare {
            let at = format!("check.prepare {:?}", h.name);
            name_problems(&at, &h.name, &mut seen, &mut problems);
            command_problems(root, &at, &h.run, &mut problems);
            platform_list_problems(&at, h.platforms.as_deref(), &declared, &mut problems);
        }
        let mut seen = BTreeSet::new();
        for s in &self.check.steps {
            let at = format!("check.steps {:?}", s.name);
            name_problems(&at, &s.name, &mut seen, &mut problems);
            command_problems(root, &at, &s.run, &mut problems);
        }
        let mut seen = BTreeSet::new();
        for a in &self.merge.after {
            let at = format!("merge.after {:?}", a.name);
            name_problems(&at, &a.name, &mut seen, &mut problems);
            command_problems(root, &at, &a.run, &mut problems);
            if a.commit.as_deref().is_some_and(|c| c.trim().is_empty()) {
                problems.push(format!("{at}: commit message is empty"));
            }
            for r in &a.regenerates {
                if let Err(m) = project_path(root, r) {
                    problems.push(format!("{at}: regenerates {m}"));
                } else if r.ends_with('/')
                    || Path::new(r)
                        .components()
                        .any(|c| matches!(c, Component::CurDir))
                    || root.join(r).is_dir()
                {
                    problems.push(format!(
                        "{at}: regenerates {r:?} must name one file, as git names it"
                    ));
                }
            }
        }
        if let Some(r) = &self.merge.resolver
            && r != "mergiraf"
        {
            problems.push(format!(
                "merge.resolver: {r:?} is not a known resolver (mergiraf)"
            ));
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(Failure::refused(format!(
                "{SETTINGS_PATH} is not valid:\n  {}",
                problems.join("\n  ")
            )))
        }
    }
}

/// Step, hook and tool names become log file names: short, plain, unique within their list.
fn name_problems(at: &str, name: &str, seen: &mut BTreeSet<String>, problems: &mut Vec<String>) {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if !ok {
        problems.push(format!(
            "{at}: names use letters, digits, '-', '_', '.' (at most 64)"
        ));
    }
    if !seen.insert(name.to_owned()) {
        problems.push(format!("{at}: name used twice"));
    }
}

/// A command must be non-empty; a program named by a path (it contains `/`) must exist.
fn command_problems(root: &Path, at: &str, argv: &[String], problems: &mut Vec<String>) {
    let Some(program) = argv.first() else {
        problems.push(format!("{at}: empty command"));
        return;
    };
    if program.is_empty() {
        problems.push(format!("{at}: empty program name"));
    } else if program.contains('/') {
        let p = Path::new(program);
        if p.is_absolute() {
            if !p.is_file() {
                problems.push(format!("{at}: {program} does not exist"));
            }
        } else {
            match project_path(root, program) {
                Err(m) => problems.push(format!("{at}: {m}")),
                Ok(full) if !full.is_file() => {
                    problems.push(format!("{at}: {program} does not exist"))
                }
                Ok(_) => {}
            }
        }
    }
}

fn platform_list_problems(
    at: &str,
    platforms: Option<&[String]>,
    declared: &BTreeSet<&str>,
    problems: &mut Vec<String>,
) {
    let Some(list) = platforms else { return };
    if list.is_empty() {
        problems.push(format!(
            "{at}: platforms is empty (leave it out for every platform)"
        ));
    }
    for p in list {
        if Platform::parse(p).is_none() {
            problems.push(format!("{at}: {p:?} is not a platform"));
        } else if !declared.contains(p.as_str()) {
            problems.push(format!("{at}: platform {p} has no [platforms.{p}] table"));
        }
    }
}

/// A relative path inside the checkout: no `..`, no absolute path, and it must exist.
fn project_path(root: &Path, rel: &str) -> std::result::Result<PathBuf, String> {
    let p = Path::new(rel);
    if p.is_absolute() || rel.is_empty() {
        return Err(format!(
            "{rel:?} must be a path relative to the checkout root"
        ));
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("{rel:?} leaves the checkout (`..`)"));
    }
    let full = root.join(p);
    if !full.exists() {
        return Err(format!("{rel} does not exist"));
    }
    Ok(full)
}

fn one_line(s: &str) -> String {
    s.lines().map(str::trim_end).collect::<Vec<_>>().join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_usable_as_path_segments_only() {
        for ok in ["0.1.0", "10.20.30", "1.0.0-rc.1", "0.1.0-alpha-2"] {
            assert!(validate_version(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "0.1",
            "0.1.0.0",
            "../0.1.0",
            "0.1.0/..",
            "v0.1.0",
            "0.1.0-",
            "0.1.0-a..b",
            "0.1.0+x",
            " 0.1.0",
        ] {
            assert!(validate_version(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn read_pin_ignores_other_keys() {
        assert_eq!(
            read_pin("atelier = \"0.2.0\"\nfuture_key = 1\n[x]\ny = 2\n").unwrap(),
            "0.2.0"
        );
        assert_eq!(
            read_pin("atelier = \"../../x\"").unwrap_err().exit,
            crate::exit::Exit::Refused
        );
        assert!(read_pin("integration_branch = \"main\"").is_err());
    }
}
