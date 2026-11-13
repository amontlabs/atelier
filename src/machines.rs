//! The owner's machines file, `~/.config/atelier/machines.toml` (DESIGN §2.5). Parsed and
//! validated only; choosing and using machines is module 3.6.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::exit::{Failure, Result};
use crate::platform::Platform;

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
                if m.connect_timeout_seconds.is_some() || m.path.is_some() {
                    problems.push(format!(
                        "{at}: connect_timeout_seconds and path are for remote machines only"
                    ));
                }
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
}
