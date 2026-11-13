//! `atelier init`, `atelier task` and `atelier agents` (DESIGN §3, §2.6, §2.7).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::agents::{self, AgentsFile, DESIGN_ROLES};
use crate::exit::{Failure, IoContext, Result};
use crate::fsutil;
use crate::git::{self, Checkout};
use crate::names;
use crate::platform::Platform;
use crate::preflight::Context;
use crate::records::BranchRecord;
use crate::rules;
use crate::settings::{SETTINGS_PATH, Settings};
use crate::template::{self, Template};

/// Roles the generated rules name; `atelier agents` refuses an agents file that lacks one.
pub const ROLES_NAMED_BY_RULES: &[&str] = &["build", "build-hard", "review"];

/// The templates shipped with atelier (DESIGN §2.7).
const SHIPPED: &[(&str, &str)] = &[
    ("build", include_str!("../assets/tasks/build.md")),
    ("review", include_str!("../assets/tasks/review.md")),
    (
        "review-fixes",
        include_str!("../assets/tasks/review-fixes.md"),
    ),
    ("judge", include_str!("../assets/tasks/judge.md")),
    ("scout", include_str!("../assets/tasks/scout.md")),
    ("gui", include_str!("../assets/tasks/gui.md")),
];

/// Where a project overrides or adds templates.
pub const TASKS_DIR: &str = ".atelier/tasks";

// ---------------------------------------------------------------------------------- init

/// Writes `.atelier/` (settings pinned to `version`, empty `project/*.md`, the generated rules),
/// the `AGENTS.md` block and the import lines. Nothing is left behind when a step fails.
pub fn init(checkout: &Checkout, version: &str) -> Result<Vec<String>> {
    let root = &checkout.root;
    let platform = Platform::current().ok_or_else(|| {
        Failure::refused(
            "this machine is not a platform atelier names; write .atelier/atelier.toml by hand",
        )
    })?;
    rules::check_agents_file(root)?;
    let dir = root.join(".atelier");
    fs::create_dir(&dir).ctx(dir.display())?;
    let result = (|| {
        let settings_text = format!(
            "atelier = \"{version}\"\nintegration_branch = \"main\"\n\n[platforms.{platform}]\nrun = \"check\"\n"
        );
        let path = root.join(SETTINGS_PATH);
        fsutil::write_atomic(&path, settings_text.as_bytes()).ctx(path.display())?;
        let project = root.join(rules::PROJECT_DIR);
        fs::create_dir(&project).ctx(project.display())?;
        for f in rules::PROJECT_FILES {
            let p = project.join(f);
            fs::write(&p, "").ctx(p.display())?;
        }
        let settings = Settings::load(root)?;
        rules::write(checkout, &settings)
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&dir);
    }
    result
}

// ---------------------------------------------------------------------------------- agents

fn load_agents(home: &Path) -> Result<AgentsFile> {
    let path = agents::path_in(home);
    if !path.is_file() {
        return Err(Failure::refused(format!(
            "{} does not exist: it names the agent roles (DESIGN 2.6)",
            path.display()
        )));
    }
    AgentsFile::load(&path)
}

/// The roles in design order, then any others by name.
fn ordered_roles(file: &AgentsFile) -> Vec<&String> {
    let mut names: Vec<&String> = Vec::new();
    for known in DESIGN_ROLES {
        if let Some((k, _)) = file.roles.get_key_value(*known) {
            names.push(k);
        }
    }
    for k in file.roles.keys() {
        if !DESIGN_ROLES.contains(&k.as_str()) {
            names.push(k);
        }
    }
    names
}

/// The table `atelier agents` prints: role, what it is for, template, how to start it.
pub fn agents_table(home: &Path) -> Result<String> {
    let file = load_agents(home)?;
    let missing: Vec<&str> = ROLES_NAMED_BY_RULES
        .iter()
        .copied()
        .filter(|r| !file.roles.contains_key(*r))
        .collect();
    if !missing.is_empty() {
        return Err(Failure::refused(format!(
            "the rules name the role(s) {} and {} has none of them",
            missing.join(", "),
            agents::path_in(home).display()
        )));
    }
    let mut rows = vec![[
        "role".to_owned(),
        "for".to_owned(),
        "template".to_owned(),
        "start".to_owned(),
    ]];
    for name in ordered_roles(&file) {
        let r = &file.roles[name];
        rows.push([
            name.clone(),
            r.purpose.clone(),
            r.template.clone(),
            r.start.clone(),
        ]);
    }
    let mut widths = [0usize; 3];
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in &rows {
        for (cell, w) in row.iter().zip(widths) {
            out.push_str(&format!("{cell:<w$}  "));
        }
        out.push_str(&row[3]);
        out.push('\n');
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------- task

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    #[serde(default)]
    fields: Vec<String>,
    #[serde(default)]
    optional: Vec<String>,
}

struct Loaded {
    name: String,
    header: Header,
    body: Template,
}

fn shipped(name: &str) -> Option<&'static str> {
    SHIPPED.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
}

/// A project's own template, else the shipped one.
fn load_template(root: &Path, name: &str) -> Result<Loaded> {
    let own = root.join(TASKS_DIR).join(format!("{name}.md"));
    let (text, origin) = match fs::read_to_string(&own) {
        Ok(t) => (t, own.display().to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => match shipped(name) {
            Some(t) => (t.to_owned(), format!("shipped template {name}")),
            None => {
                return Err(Failure::usage(format!(
                    "no task template {name:?}: neither {TASKS_DIR}/{name}.md nor a shipped one"
                )));
            }
        },
        Err(e) => return Err(Failure::refused(format!("{}: {e}", own.display()))),
    };
    let bad = |m: String| Failure::refused(format!("{origin}: {m}"));
    let rest = text
        .strip_prefix("+++\n")
        .ok_or_else(|| bad("must start with a `+++` header line".to_owned()))?;
    let end = rest
        .find("\n+++\n")
        .ok_or_else(|| bad("the header has no closing `+++` line".to_owned()))?;
    let header: Header = toml::from_str(&rest[..end])
        .map_err(|e| bad(format!("header: {}", e.to_string().trim())))?;
    let body = Template::parse(&rest[end + 5..]).map_err(bad)?;
    for f in header.fields.iter().chain(&header.optional) {
        if !template::valid_name(f) {
            return Err(bad(format!("header field {f:?} is not a field name")));
        }
    }
    Ok(Loaded {
        name: name.to_owned(),
        header,
        body,
    })
}

/// `field=value` or `field=@file` arguments, files read relative to `cwd`.
fn parse_args(cwd: &Path, args: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for a in args {
        let (name, value) = a
            .split_once('=')
            .ok_or_else(|| Failure::usage(format!("{a:?} is not field=value or field=@file")))?;
        if !template::valid_name(name) {
            return Err(Failure::usage(format!("{name:?} is not a field name")));
        }
        let value = match value.strip_prefix('@') {
            Some(file) => {
                let p = cwd.join(file);
                fs::read_to_string(&p)
                    .map_err(|e| Failure::usage(format!("{name}=@{file}: {}: {e}", p.display())))?
                    .trim_end_matches('\n')
                    .to_owned()
            }
            None => value.to_owned(),
        };
        if out.insert(name.to_owned(), value).is_some() {
            return Err(Failure::usage(format!("field {name} is given twice")));
        }
    }
    Ok(out)
}

/// The worktree that has `branch` checked out.
fn worktree_of(root: &Path, branch: &str) -> Result<Option<PathBuf>> {
    let list = git::run(root, ["worktree", "list", "--porcelain"])?;
    let want = names::branch_ref(branch);
    let mut current: Option<PathBuf> = None;
    for line in list.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            current = Some(PathBuf::from(p));
        } else if line.strip_prefix("branch ") == Some(want.as_str()) {
            return Ok(current);
        }
    }
    Ok(None)
}

/// The values atelier fills itself.
fn known_values(ctx: &Context, branch: Option<&str>) -> Result<BTreeMap<String, String>> {
    let root = &ctx.checkout.root;
    let mut known = BTreeMap::new();
    let Some(branch) = branch else {
        known.insert(
            "rules".to_owned(),
            root.join(rules::RULES_DIR).display().to_string(),
        );
        return Ok(known);
    };
    names::validate_branch(root, branch)?;
    let record = BranchRecord::read(&ctx.state, branch)?.ok_or_else(|| {
        Failure::usage(format!(
            "branch {branch} has no record: start it with atelier start"
        ))
    })?;
    let worktree = worktree_of(root, branch)?.ok_or_else(|| {
        Failure::usage(format!(
            "branch {branch} is not checked out in any worktree: start it with atelier start --path"
        ))
    })?;
    known.insert("branch".to_owned(), branch.to_owned());
    known.insert(
        "rules".to_owned(),
        worktree.join(rules::RULES_DIR).display().to_string(),
    );
    known.insert("worktree".to_owned(), worktree.display().to_string());
    known.insert("base".to_owned(), record.base.clone());
    known.insert("lower".to_owned(), record.lower.clone());
    known.insert(
        "diff".to_owned(),
        format!("git diff {}..{branch}", record.base),
    );
    if record.lower != ctx.settings.integration_branch {
        known.insert("stacked".to_owned(), "yes".to_owned());
    }
    Ok(known)
}

/// Names atelier can fill when a branch is given.
const BRANCH_KNOWN: &[&str] = &[
    "branch", "worktree", "base", "lower", "diff", "rules", "stacked",
];

/// Renders the task for `role` into `.git/atelier/tasks/<id>.md`; returns the file's path.
pub fn task(
    ctx: &Context,
    home: &Path,
    cwd: &Path,
    role: &str,
    branch: &str,
    args: &[String],
    fixes: bool,
) -> Result<PathBuf> {
    let agents = load_agents(home)?;
    let role_entry = agents.roles.get(role).ok_or_else(|| {
        Failure::usage(format!(
            "no role {role:?} in {}; roles: {}",
            agents::path_in(home).display(),
            agents.roles.keys().cloned().collect::<Vec<_>>().join(", ")
        ))
    })?;
    let template_name = if fixes {
        format!("{}-fixes", role_entry.template)
    } else {
        role_entry.template.clone()
    };
    let given = parse_args(cwd, args)?;
    let tpl = load_template(&ctx.checkout.root, &template_name)?;

    let branch = (branch != "-").then_some(branch);
    let known = known_values(ctx, branch)?;

    let declared: BTreeSet<&str> = tpl
        .header
        .fields
        .iter()
        .chain(&tpl.header.optional)
        .map(String::as_str)
        .collect();
    let mut problems = Vec::new();
    let unknown: Vec<&str> = given
        .keys()
        .map(String::as_str)
        .filter(|k| !declared.contains(k))
        .collect();
    if !unknown.is_empty() {
        problems.push(format!(
            "unknown field(s) for template {}: {}",
            tpl.name,
            unknown.join(", ")
        ));
    }
    let filled: Vec<&str> = given
        .keys()
        .map(String::as_str)
        .filter(|k| declared.contains(k) && (BRANCH_KNOWN.contains(k)))
        .collect();
    if !filled.is_empty() {
        problems.push(format!(
            "atelier fills {} itself; remove it from the arguments",
            filled.join(", ")
        ));
    }
    let missing: Vec<&str> = tpl
        .header
        .fields
        .iter()
        .map(String::as_str)
        .filter(|f| !given.contains_key(*f))
        .collect();
    if !missing.is_empty() {
        problems.push(format!(
            "missing field(s) for template {}: {}",
            tpl.name,
            missing.join(", ")
        ));
    }
    for used in tpl.body.names() {
        if declared.contains(used.as_str()) || known.contains_key(&used) {
            continue;
        }
        if BRANCH_KNOWN.contains(&used.as_str()) {
            // `stacked` is simply unset for an item built on the integration branch.
            if used != "stacked" || branch.is_none() {
                problems.push(format!(
                    "template {} uses {{{{{used}}}}}, which needs a branch (this task has none: `-`)",
                    tpl.name
                ));
            }
        } else {
            return Err(Failure::refused(format!(
                "template {} uses {{{{{used}}}}}, which its header does not declare",
                tpl.name
            )));
        }
    }
    if !problems.is_empty() {
        return Err(Failure::usage(problems.join("\n")));
    }

    let mut values = known;
    values.extend(given);
    let text = tpl.body.render(&values);

    let tasks = ctx.state.root().join("tasks");
    fs::create_dir_all(&tasks).ctx(tasks.display())?;
    let (y, mo, d, h, mi, s) = fsutil::utc_parts(fsutil::now_secs());
    let id = format!(
        "{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z-{}",
        fsutil::random_hex(4)?
    );
    let path = tasks.join(format!("{id}.md"));
    fsutil::write_atomic(&path, text.as_bytes()).ctx(path.display())?;
    Ok(path)
}

/// The one line the orchestrator passes as the agent's prompt.
pub fn prompt_line(path: &Path) -> String {
    format!("Read {}: it is your task.", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_templates_are_well_formed() {
        let tmp = std::env::temp_dir().join("atelier-no-such-project");
        for (name, _) in SHIPPED {
            let t = load_template(&tmp, name).unwrap_or_else(|e| panic!("{name}: {e}"));
            let declared: BTreeSet<String> = t
                .header
                .fields
                .iter()
                .chain(&t.header.optional)
                .cloned()
                .collect();
            for used in t.body.names() {
                assert!(
                    declared.contains(&used) || BRANCH_KNOWN.contains(&used.as_str()),
                    "{name}: {used}"
                );
            }
        }
    }
}
