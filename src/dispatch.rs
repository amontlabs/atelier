//! The dispatcher and the pin (DESIGN §2.1).
//!
//! One binary plays both parts. Started without `ATELIER_DISPATCHED` in its environment, it is
//! the dispatcher: it reads the pin from `.atelier/atelier.toml` of the checkout it runs in and
//! runs `~/.local/share/atelier/<pin>/bin/atelier` with `ATELIER_DISPATCHED=1`, or runs in place
//! when it is that file itself. A missing version prints the one install command and exits 4;
//! nothing installs itself. Outside a project only `atelier init --version <v>`,
//! `atelier --version` (and help) and `atelier hook …` run. The dispatched program removes the variable from its own
//! environment, so nothing it starts (a nested atelier in a gate, for one) skips dispatch, and
//! it compares its own version with the pin anyway (preflight), so calling a binary directly
//! cannot bypass the check.
//!
//! The dispatcher reads only the `atelier` key, so a settings file written for a newer version
//! still dispatches to that version.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::exit::{Failure, Result};
use crate::git::{self, Checkout};
use crate::settings::{self, SETTINGS_PATH};

/// Set by the dispatcher on the program it runs.
pub const DISPATCHED_VAR: &str = "ATELIER_DISPATCHED";

/// This program's version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where atelier's source is installed from.
pub const REPO_URL: &str = "https://github.com/amontlabs/atelier";

/// `~/.local/share/atelier`.
pub fn install_root(home: &Path) -> PathBuf {
    home.join(".local/share/atelier")
}

/// The installed binary of a version.
pub fn binary_for(home: &Path, version: &str) -> PathBuf {
    install_root(home).join(version).join("bin/atelier")
}

/// The one command that installs a version.
pub fn install_command(version: &str) -> String {
    format!(
        "cargo install --git {REPO_URL} --tag v{version} --locked --root ~/.local/share/atelier/{version}"
    )
}

/// The refusal for a version that is not installed.
pub fn not_installed(version: &str) -> Failure {
    Failure::refused(format!(
        "atelier {version} is not installed. Install it with:\n  {}",
        install_command(version)
    ))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// This binary is the right one: go on.
    RunHere,
    /// Run this file instead.
    Exec(PathBuf),
}

/// What the arguments ask for, as far as the dispatcher needs to know.
#[derive(Debug, PartialEq, Eq)]
enum Ask {
    /// `--version`, `-V`, help, or nothing at all.
    Harmless,
    /// `init --version <v>`.
    Init(Option<String>),
    Other,
}

fn classify(args: &[OsString]) -> Ask {
    let rest: Vec<&str> = args
        .iter()
        .skip(1)
        .map(|a| a.to_str().unwrap_or("\u{0}"))
        .collect();
    // `--help` of any command only prints text.
    if rest
        .iter()
        .take_while(|a| **a != "--")
        .any(|a| *a == "--help" || *a == "-h")
    {
        return Ask::Harmless;
    }
    match rest.as_slice() {
        [] | ["--version"] | ["-V"] | ["--help"] | ["-h"] | ["help", ..] => Ask::Harmless,
        // A harness hook runs in any folder; outside a project it only lets the agent stop.
        ["hook", ..] => Ask::Harmless,
        ["init", tail @ ..] => {
            let mut version = None;
            let mut it = tail.iter();
            while let Some(a) = it.next() {
                if *a == "--version" {
                    version = it.next().map(|v| (*v).to_owned());
                } else if let Some(v) = a.strip_prefix("--version=") {
                    version = Some(v.to_owned());
                }
            }
            Ask::Init(version)
        }
        _ => Ask::Other,
    }
}

/// Decides which binary runs. `current_exe` is this program's file.
pub fn decide(
    args: &[OsString],
    cwd: &Path,
    home: Option<&Path>,
    current_exe: &Path,
) -> Result<Decision> {
    let ask = classify(args);
    let pin = match Checkout::find(cwd)? {
        Some(c) if c.root.join(SETTINGS_PATH).is_file() => {
            let text = std::fs::read_to_string(c.root.join(SETTINGS_PATH))
                .map_err(|e| Failure::refused(format!("{SETTINGS_PATH}: {e}")))?;
            Some(settings::read_pin(&text)?)
        }
        _ => None,
    };
    let target = match (pin, ask) {
        (Some(pin), _) => pin,
        (None, Ask::Harmless) => return Ok(Decision::RunHere),
        (None, Ask::Init(Some(v))) => {
            settings::validate_version(&v).map_err(Failure::usage)?;
            v
        }
        (None, Ask::Init(None)) => {
            return Err(Failure::usage(
                "outside a project, name the version: atelier init --version <version>",
            ));
        }
        (None, Ask::Other) => {
            return Err(Failure::refused(format!(
                "not in an atelier project (no {SETTINGS_PATH} in this checkout): only `atelier init --version <version>` and `atelier --version` run here"
            )));
        }
    };
    let home = home.ok_or_else(|| Failure::refused("HOME is not set"))?;
    let binary = binary_for(home, &target);
    if !binary.is_file() {
        return Err(not_installed(&target));
    }
    if git::canonical(&binary) == git::canonical(current_exe) {
        Ok(Decision::RunHere)
    } else {
        Ok(Decision::Exec(binary))
    }
}

/// Replaces this process with `binary`, marked as dispatched. Returns only on failure.
pub fn exec(binary: &Path, args: &[OsString]) -> Failure {
    let err = Command::new(binary)
        .args(args.iter().skip(1))
        .env(DISPATCHED_VAR, "1")
        .exec();
    Failure::refused(format!("cannot run {}: {err}", binary.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<OsString> {
        std::iter::once("atelier")
            .chain(a.iter().copied())
            .map(OsString::from)
            .collect()
    }

    #[test]
    fn classifies_what_runs_outside_a_project() {
        assert_eq!(classify(&args(&["--version"])), Ask::Harmless);
        assert_eq!(classify(&args(&[])), Ask::Harmless);
        assert_eq!(classify(&args(&["init", "--help"])), Ask::Harmless);
        assert_eq!(classify(&args(&["clean", "-h"])), Ask::Harmless);
        assert_eq!(classify(&args(&["tmp", "--", "ls", "-h"])), Ask::Other);
        assert_eq!(
            classify(&args(&["init", "--version", "0.2.0"])),
            Ask::Init(Some("0.2.0".into()))
        );
        assert_eq!(
            classify(&args(&["init", "--version=0.2.0"])),
            Ask::Init(Some("0.2.0".into()))
        );
        assert_eq!(classify(&args(&["init"])), Ask::Init(None));
        assert_eq!(classify(&args(&["check"])), Ask::Other);
        assert_eq!(classify(&args(&["hook", "stop"])), Ask::Harmless);
        assert_eq!(classify(&args(&["--version", "check"])), Ask::Other);
    }

    #[test]
    fn install_command_names_the_version_folder() {
        assert_eq!(
            install_command("0.1.0"),
            "cargo install --git https://github.com/amontlabs/atelier --tag v0.1.0 --locked --root ~/.local/share/atelier/0.1.0"
        );
    }
}
