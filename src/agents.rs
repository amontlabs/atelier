//! The owner's agents file, `~/.config/atelier/agents.toml` (DESIGN §2.6). Parsed and validated
//! only; `atelier agents` and `atelier task` are module 3.7.
//!
//! ```toml
//! [roles.build]
//! purpose = "Implementation and fixes, one topic per agent"
//! start = "lastdrop agent `build` through delegate_task or Agent"
//! template = "build"
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::exit::{Failure, Result};

/// The roles DESIGN §2.6 names. Another owner's file may name others.
pub const DESIGN_ROLES: &[&str] = &[
    "build",
    "build-hard",
    "review",
    "judge",
    "scout",
    "gui",
    "cheap",
];

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AgentsFile {
    #[serde(default)]
    pub roles: BTreeMap<String, Role>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Role {
    /// What the role is for.
    pub purpose: String,
    /// How the orchestrator starts it in this owner's harness.
    pub start: String,
    /// Its default task template (2.7).
    pub template: String,
}

pub fn path_in(home: &Path) -> PathBuf {
    home.join(".config/atelier/agents.toml")
}

impl AgentsFile {
    pub fn parse(text: &str) -> Result<AgentsFile> {
        let file: AgentsFile = toml::from_str(text)
            .map_err(|e| Failure::refused(format!("agents.toml: {}", e.to_string().trim())))?;
        let mut problems = Vec::new();
        for (name, role) in &file.roles {
            let name_ok = !name.is_empty()
                && name.as_bytes()[0].is_ascii_lowercase()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !name_ok {
                problems.push(format!("roles.{name}: role names use a-z, 0-9 and '-'"));
            }
            for (field, value) in [
                ("purpose", &role.purpose),
                ("start", &role.start),
                ("template", &role.template),
            ] {
                if value.trim().is_empty() {
                    problems.push(format!("roles.{name}.{field} is empty"));
                }
            }
            if !role
                .template
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            {
                problems.push(format!(
                    "roles.{name}.template: {:?} is not a template name",
                    role.template
                ));
            }
        }
        if problems.is_empty() {
            Ok(file)
        } else {
            Err(Failure::refused(format!(
                "agents.toml is not valid:\n  {}",
                problems.join("\n  ")
            )))
        }
    }

    pub fn load(path: &Path) -> Result<AgentsFile> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Failure::refused(format!("{}: {e}", path.display())))?;
        Self::parse(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_roles() {
        let f = AgentsFile::parse(
            "[roles.build]\npurpose = \"p\"\nstart = \"s\"\ntemplate = \"build\"\n[roles.review]\npurpose = \"r\"\nstart = \"s\"\ntemplate = \"review\"\n",
        )
        .unwrap();
        assert_eq!(f.roles.len(), 2);
        assert_eq!(f.roles["review"].template, "review");
    }

    #[test]
    fn rejects_unknown_keys_and_empty_fields() {
        for bad in [
            "[roles.build]\npurpose = \"p\"\nstart = \"s\"\ntemplate = \"build\"\nmodel = \"x\"\n",
            "[roles.build]\npurpose = \"\"\nstart = \"s\"\ntemplate = \"build\"\n",
            "[roles.Build]\npurpose = \"p\"\nstart = \"s\"\ntemplate = \"build\"\n",
            "[roles.build]\npurpose = \"p\"\nstart = \"s\"\ntemplate = \"../x\"\n",
            "[roles.build]\npurpose = \"p\"\nstart = \"s\"\n",
            "[agents]\n",
        ] {
            assert!(AgentsFile::parse(bad).is_err(), "{bad}");
        }
    }
}
