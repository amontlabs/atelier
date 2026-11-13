//! The command line (DESIGN §3). Every command is declared; those not built yet run their
//! preflight and then exit 2 with `not built yet`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{ArgGroup, Parser, Subcommand};

use crate::dispatch::{self, DISPATCHED_VAR, Decision, VERSION};
use crate::exit::{Exit, Failure, Result};
use crate::git::Checkout;
use crate::preflight::{self, Needs};
use crate::remote_run::{self, RemoteAction};
use crate::rules;
use crate::settings::{self, SETTINGS_PATH};
use crate::state::State;
use crate::task;
use crate::testcmd;

#[derive(Debug, Parser)]
#[command(
    name = "atelier",
    version,
    about = "Gate, records and guards for agents building one project in parallel git worktrees"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Set up atelier in this project (once, by the owner).
    Init {
        /// The atelier version to pin; required outside a project.
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
    },
    /// Rewrite the generated rules from the settings and `.atelier/project/`.
    Rules,
    /// Prove the tree in this worktree.
    Check {
        /// Format, compile, and the steps marked `compile = true` only.
        #[arg(long)]
        compile_only: bool,
        /// Re-run only the failures listed in this file (the gate's bisection).
        #[arg(
            long,
            value_name = "FILE",
            hide = true,
            conflicts_with = "compile_only"
        )]
        only: Option<PathBuf>,
    },
    /// Create a branch and record what it is built on.
    Start {
        branch: String,
        /// The branch it is built on (default: the integration branch).
        #[arg(long, value_name = "LOWER")]
        on: Option<String>,
        /// Also create a worktree for it here.
        #[arg(long, value_name = "WORKTREE")]
        path: Option<PathBuf>,
        /// Only undo an earlier start of this branch that did not finish.
        #[arg(long, conflicts_with_all = ["on", "path"])]
        abandon: bool,
    },
    /// Rebase this branch's own commits onto what it is built on.
    Follow,
    /// Record that a branch's exact commit was reviewed and may be merged.
    Approve { branch: String, commit: String },
    /// Merge approved branches through the gate and move the integration branch if green.
    Merge {
        #[arg(required = true)]
        branches: Vec<String>,
        /// A platform the owner waived for this merge; repeat it for several, each with its
        /// own --reason, in the same order.
        #[arg(long, value_name = "PLATFORM", requires = "reason")]
        waive: Vec<String>,
        /// The owner's words for the waiver named by the --waive in the same position.
        #[arg(long, requires = "waive")]
        reason: Vec<String>,
    },
    /// Finish a pending promotion.
    Promote {
        /// Drop the pending promotion; the approvals stay.
        #[arg(long)]
        discard: bool,
    },
    /// Run a command with a registered temp folder in ATELIER_TMP.
    Tmp {
        /// A short folder under /tmp, for socket paths.
        #[arg(long)]
        short: bool,
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Delete what was merged and what atelier made.
    Clean {
        branches: Vec<String>,
        /// Execute the plan instead of printing it.
        #[arg(long)]
        yes: bool,
        /// Let these commits go: work the branch's reflog alone remembers, which clean listed.
        #[arg(long, num_args = 1.., value_name = "COMMIT")]
        discard: Vec<String>,
    },
    /// Run a platform's check, or a given command, on a machine of that platform.
    #[command(group(ArgGroup::new("which").required(true).args(["platform", "all"])))]
    Test {
        platform: Option<String>,
        /// Every platform of the project.
        #[arg(long)]
        all: bool,
        #[arg(last = true, value_name = "COMMAND")]
        command: Vec<String>,
    },
    /// Render a role's task template into a file the agent reads.
    Task {
        role: String,
        /// The branch, or `-` for tasks without one.
        branch: String,
        /// `field=value` or `field=@file`.
        fields: Vec<String>,
        /// Use the review-fixes template.
        #[arg(long)]
        fixes: bool,
    },
    /// Print the roles from ~/.config/atelier/agents.toml.
    Agents,
    /// Read-only view of the preflight and of what the commands recorded.
    Status {
        /// Print the sections as JSON (name, lines, omitted) instead of text.
        #[arg(long)]
        json: bool,
    },
    /// List the machines, or set one up for this project.
    Machines {
        #[command(subcommand)]
        action: Option<MachinesCmd>,
    },
    /// Hooks the agent harnesses run (registered by `init` and `rules`).
    Hook {
        #[command(subcommand)]
        action: HookCmd,
    },
    /// Internal: the machine side of a run started over ssh by `test` or `merge`.
    #[command(hide = true)]
    RemoteRun {
        #[command(subcommand)]
        action: RemoteAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum MachinesCmd {
    /// Create this project's checkout on a machine and install the pinned atelier there. Prints
    /// the plan; replacing an installed atelier of other source needs --yes.
    Init {
        name: String,
        /// Replace an installed atelier built from other source.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum HookCmd {
    /// Claude Code's and Codex's Stop and SubagentStop hook: reads the hook's JSON on stdin and
    /// blocks the end of a worker's turn in an atelier worktree while work is unfinished.
    Stop,
}

impl Cmd {
    pub fn name(&self) -> &'static str {
        match self {
            Cmd::Init { .. } => "init",
            Cmd::Rules => "rules",
            Cmd::Check { .. } => "check",
            Cmd::Start { .. } => "start",
            Cmd::Follow => "follow",
            Cmd::Approve { .. } => "approve",
            Cmd::Merge { .. } => "merge",
            Cmd::Promote { .. } => "promote",
            Cmd::Tmp { .. } => "tmp",
            Cmd::Clean { .. } => "clean",
            Cmd::Test { .. } => "test",
            Cmd::Task { .. } => "task",
            Cmd::Agents => "agents",
            Cmd::Status { .. } => "status",
            Cmd::Machines { .. } => "machines",
            Cmd::Hook { .. } => "hook",
            Cmd::RemoteRun { .. } => "remote-run",
        }
    }

    /// The module that builds it.
    fn module(&self) -> &'static str {
        match self {
            Cmd::Check { .. } => "3.2",
            Cmd::Start { .. } | Cmd::Follow => "3.3",
            Cmd::Approve { .. } | Cmd::Merge { .. } | Cmd::Promote { .. } => "3.4",
            Cmd::Tmp { .. } | Cmd::Clean { .. } => "3.5",
            Cmd::Test { .. } | Cmd::Machines { .. } | Cmd::RemoteRun { .. } => "3.6",
            Cmd::Init { .. } | Cmd::Rules | Cmd::Task { .. } | Cmd::Agents => "3.7",
            Cmd::Status { .. } => "3.8",
            Cmd::Hook { .. } => "3.10",
        }
    }

    fn needs(&self) -> Needs {
        match self {
            Cmd::Init { .. } | Cmd::Rules | Cmd::Status { .. } => Needs::default(),
            Cmd::Approve { .. } | Cmd::Merge { .. } | Cmd::Promote { .. } | Cmd::Clean { .. } => {
                Needs {
                    rules: true,
                    versioned: true,
                }
            }
            _ => Needs {
                rules: true,
                versioned: false,
            },
        }
    }
}

/// The program's entry point; returns the exit code.
pub fn main() -> i32 {
    let args: Vec<OsString> = std::env::args_os().collect();
    // A launchd job's shim: the same program, never dispatched (DESIGN §8.4).
    #[cfg(target_os = "macos")]
    if args.get(1).is_some_and(|a| a == crate::launchd::SHIM_ARG) {
        return crate::launchd::shim_main(&args);
    }
    match main_with(&args) {
        Ok(exit) => exit.code(),
        Err(f) => {
            if !f.message.is_empty() {
                eprintln!("atelier: {}", f.message);
            }
            f.exit.code()
        }
    }
}

fn main_with(args: &[OsString]) -> Result<Exit> {
    let cwd =
        std::env::current_dir().map_err(|e| Failure::refused(format!("current folder: {e}")))?;
    if std::env::var_os(DISPATCHED_VAR).is_none() {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let exe =
            std::env::current_exe().map_err(|e| Failure::refused(format!("own path: {e}")))?;
        match dispatch::decide(args, &cwd, home.as_deref(), &exe)? {
            Decision::RunHere => {}
            Decision::Exec(binary) => return Err(dispatch::exec(&binary, args)),
        }
    } else {
        // SAFETY: single-threaded at this point; nothing else reads the environment yet.
        unsafe { std::env::remove_var(DISPATCHED_VAR) };
    }

    let cli = match Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            let code = if e.use_stderr() {
                Exit::Usage
            } else {
                Exit::Ok
            };
            let _ = e.print();
            return Ok(code);
        }
    };
    run(cli.command, &cwd)
}

/// Runs a parsed command in `cwd`.
pub fn run(cmd: Cmd, cwd: &Path) -> Result<Exit> {
    match cmd {
        Cmd::RemoteRun { action } => return remote_run::main(action),
        // No preflight: a hook must answer whatever state the project is in, and never refuse.
        Cmd::Hook {
            action: HookCmd::Stop,
        } => {
            crate::hook::stop_main();
            return Ok(Exit::Ok);
        }
        Cmd::Test {
            platform,
            all,
            command,
        } => {
            let ctx = preflight::run(
                cwd,
                "test",
                Cmd::Test {
                    platform: None,
                    all,
                    command: Vec::new(),
                }
                .needs(),
            )?;
            return testcmd::test(&ctx, cwd, platform.as_deref(), all, command);
        }
        Cmd::Machines { action } => {
            let ctx = preflight::run(cwd, "machines", Cmd::Machines { action: None }.needs())?;
            return match action {
                None => testcmd::list(&ctx),
                Some(MachinesCmd::Init { name, yes }) => testcmd::init(&ctx, &name, yes),
            };
        }
        _ => {}
    }
    match &cmd {
        Cmd::Init { version } => {
            init_preflight(cwd, version.as_deref())?;
            let checkout = Checkout::find(cwd)?.expect("checked by init_preflight");
            let changed = task::init(&checkout, version.as_deref().unwrap_or(VERSION))?;
            println!(
                "initialized atelier {} in {}",
                version.as_deref().unwrap_or(VERSION),
                checkout.root.display()
            );
            for line in changed {
                println!("  {line}");
            }
            return Ok(Exit::Ok);
        }
        Cmd::Rules => {
            let context = preflight::run(cwd, cmd.name(), cmd.needs())?;
            let changed = rules::write(&context.checkout, &context.settings)?;
            if changed.is_empty() {
                println!("rules are current");
            }
            for line in changed {
                println!("{line}");
            }
            return Ok(Exit::Ok);
        }
        Cmd::Agents => {
            let _context = preflight::run(cwd, cmd.name(), cmd.needs())?;
            print!("{}", task::agents_table(&home()?)?);
            return Ok(Exit::Ok);
        }
        Cmd::Task {
            role,
            branch,
            fields,
            fixes,
        } => {
            let context = preflight::run(cwd, cmd.name(), cmd.needs())?;
            let path = task::task(&context, &home()?, cwd, role, branch, fields, *fixes)?;
            println!("{}", task::prompt_line(&path));
            return Ok(Exit::Ok);
        }
        Cmd::Status { json } => return crate::status::run(cwd, *json),
        Cmd::Check { compile_only, only } => {
            let context = preflight::run(cwd, cmd.name(), cmd.needs())?;
            let only = match only {
                Some(f) => Some(crate::check::Only::read(f)?),
                None => None,
            };
            return crate::check::run_mode(&context, *compile_only, only, &mut None);
        }
        Cmd::Approve { branch, commit } => {
            let ctx = preflight::run(cwd, cmd.name(), cmd.needs())?;
            return crate::approve::run(&ctx, branch, commit);
        }
        Cmd::Merge {
            branches,
            waive,
            reason,
        } => {
            let ctx = preflight::run(cwd, cmd.name(), cmd.needs())?;
            if waive.len() != reason.len() {
                return Err(crate::exit::Failure::usage(format!(
                    "each --waive needs its own --reason ({} --waive, {} --reason)",
                    waive.len(),
                    reason.len()
                )));
            }
            let waivers: Vec<crate::gate::Waiver> = waive
                .iter()
                .zip(reason)
                .map(|(platform, reason)| crate::gate::Waiver {
                    platform: platform.clone(),
                    reason: reason.clone(),
                })
                .collect();
            return crate::gate::run(&ctx, branches, &waivers);
        }
        Cmd::Promote { discard } => {
            let ctx = preflight::run(cwd, cmd.name(), cmd.needs())?;
            return crate::promote::run(&ctx, *discard);
        }
        Cmd::Start {
            branch,
            on,
            path,
            abandon,
        } => {
            let ctx = preflight::run(cwd, cmd.name(), cmd.needs())?;
            return crate::start::run(&ctx, cwd, branch, on.as_deref(), path.as_deref(), *abandon);
        }
        Cmd::Follow => {
            let ctx = preflight::run(cwd, cmd.name(), cmd.needs())?;
            return crate::follow::run(&ctx);
        }
        Cmd::Tmp { short, command } => {
            let ctx = preflight::run(cwd, cmd.name(), cmd.needs())?;
            return crate::tmp::run(&ctx, cwd, *short, command);
        }
        Cmd::Clean {
            branches,
            yes,
            discard,
        } => {
            let ctx = preflight::run(cwd, cmd.name(), cmd.needs())?;
            return crate::clean::run(&ctx, branches, *yes, discard);
        }
        _ => {
            let _context = preflight::run(cwd, cmd.name(), cmd.needs())?;
        }
    }
    Err(Failure::usage(format!(
        "{}: not built yet (module {})",
        cmd.name(),
        cmd.module()
    )))
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Failure::refused("HOME is not set"))
}

/// `init` runs outside a project: it needs a git worktree without `.atelier/`, and its own version
/// must be the one named.
fn init_preflight(cwd: &Path, version: Option<&str>) -> Result<()> {
    if let Some(v) = version {
        settings::validate_version(v).map_err(Failure::usage)?;
        if v != VERSION {
            return Err(Failure::refused(format!(
                "init --version {v} reached atelier {VERSION}; install {v} with:\n  {}",
                dispatch::install_command(v)
            )));
        }
    }
    let checkout = Checkout::find(cwd)?
        .ok_or_else(|| Failure::refused(format!("{} is not in a git worktree", cwd.display())))?;
    if checkout.root.join(".atelier").exists() {
        let pinned = checkout.root.join(SETTINGS_PATH);
        return Err(Failure::refused(if pinned.is_file() {
            ".atelier/ exists: this project is already set up".to_owned()
        } else {
            ".atelier/ exists: init writes it only in a project that has none".to_owned()
        }));
    }
    let _state = State::open(&checkout)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn every_design_command_parses() {
        for line in [
            "atelier init --version 0.1.0",
            "atelier rules",
            "atelier check",
            "atelier check --compile-only",
            "atelier start disk/api --on main --path ../wt",
            "atelier follow",
            "atelier approve disk-api 3f2a9c1",
            "atelier merge disk-api disk-ui --waive linux-x86_64 --reason owner",
            "atelier merge a --waive linux-x86_64 --reason r1 --waive macos-aarch64 --reason r2",
            "atelier promote --discard",
            "atelier tmp --short -- sh -c true",
            "atelier clean a b --yes",
            "atelier test linux-x86_64 -- cargo nextest run -p x",
            "atelier test --all",
            "atelier task review disk-api --fixes x=1 y=@f",
            "atelier task judge -",
            "atelier agents",
            "atelier status",
            "atelier machines",
            "atelier machines init linux-box",
            "atelier hook stop",
        ] {
            let args: Vec<&str> = line.split(' ').collect();
            Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{line}: {e}"));
        }
    }

    #[test]
    fn wrong_usage_is_rejected() {
        for line in [
            "atelier",
            "atelier merge",
            "atelier merge a --waive linux-x86_64",
            "atelier test",
            "atelier test linux-x86_64 --all",
            "atelier tmp",
            "atelier approve a",
            "atelier nope",
        ] {
            let args: Vec<&str> = line.split(' ').collect();
            let e = Cli::try_parse_from(&args).unwrap_err();
            assert!(e.use_stderr(), "{line}");
        }
    }
}
